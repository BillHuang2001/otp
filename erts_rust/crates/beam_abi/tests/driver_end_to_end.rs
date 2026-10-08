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

//! End-to-end proof of the driver C ABI (workspace contract **A.4**): a **real C driver shared
//! object** — compiled with the system `cc` against the system `erl_driver.h` — is `dlopen`ed,
//! validated, and driven through the Rust loader, with its `driver_*` / `erl_drv_*` imports
//! resolving out of the *test binary's* dynamic symbol table.
//!
//! # Why this is the load-bearing test
//!
//! `tests/fixtures/test_driver.c` is an ordinary driver: it `#include`s `erl_driver.h` and calls
//! `driver_alloc`, `driver_free`, `driver_output`, `driver_set_timer`, … whose definitions live in
//! *this* crate (`beam_abi::driver::drv`). The linker leaves those calls **undefined** in the `.so`
//! (verified below by inspecting the `.so`'s dynamic symbol table), so `dlopen(..., RTLD_NOW)` in
//! [`beam_abi::driver::loader::load`] can only succeed if the *loading process* exports them as
//! dynamic symbols. That is what `build.rs` arranges with `-Wl,--export-dynamic` on the test
//! targets; `the_test_binary_exports_the_driver_symbols_dynamically` pins the mechanism directly,
//! and the successful load + the recorded `driver_output` traffic in `loads_and_drives_a_real_c_driver`
//! are the end-to-end consequence.
//!
//! # Honest scope of the proof
//!
//! The **real** end-to-end claims are: (1) a driver `.so` built against the unmodified header
//! `dlopen`s and validates, and (2) its callbacks drive the Rust `driver_*` host functions, whose
//! effects land in a concrete [`beam_abi::DriverHost`] (the recording `TestDriverHost`). The
//! **shape** claims are the `ErlDrvEntry` field read-back and the symbol-table inspection. What is
//! *not* proven here is a real scheduler/poller behind the seam: the host only records the
//! `select`/`timer`/`async` requests (returning success), so "the port was really woken by the OS
//! poller" is a `beam_io`/`beam_rt` concern, out of this crate's scope.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`); inspecting symbol tables needs `nm`/`readelf`. When a *toolchain* facility is
//! missing the affected test prints a loud `SKIP:` banner and returns — but when the toolchain **is**
//! present, every assertion is hard (a mismatch fails, it never silently passes).

mod common;

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::{load, DriverLoadError};
use beam_abi::driver::types::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MARKER, ERL_DRV_EXTENDED_MINOR_VERSION,
    ERL_DRV_READ, ERL_DRV_USE,
};
use beam_abi::DriverEnv;
use beam_term::prelude::{
    atom_val, is_atom, is_small, is_tuple, signed_value, tuple_element, tuple_size,
};
use libc::c_char;

use common::{compile_so, skip, TestDriverHost};

/// The committed C fixture — a real driver (see the file header for the A.4 rationale).
const FIXTURE_C: &str = include_str!("fixtures/test_driver.c");

/// The `driver_*` / `erl_drv_*` symbols the fixture imports; they must be *undefined* in the `.so`
/// and *defined* in the loading process.
const FIXTURE_IMPORTS: [&str; 11] = [
    "driver_alloc",
    "driver_free",
    "driver_output",
    "driver_set_timer",
    "driver_cancel_timer",
    "driver_select",
    "driver_async",
    "driver_mk_atom",
    "driver_mk_port",
    "erl_drv_output_term",
    "driver_system_info",
];

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

/// Run `cmd args…`, returning its stdout on success (`None` when the tool is missing or fails).
fn run_tool(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `Some(true)`/`Some(false)` when a symbol-dump tool inspected `path`, `None` when neither `nm`
/// nor `readelf` is usable. `true` iff every name in `names` is an **undefined** import of `path`.
fn all_symbols_are_undefined_imports(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--undefined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && line.contains(" UND "))
        }));
    }
    None
}

/// As [`all_symbols_are_undefined_imports`], but every name must be **defined** in `path`'s
/// dynamic symbol table (i.e. exported by it).
fn all_symbols_are_exported(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--defined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && !line.contains(" UND "))
        }));
    }
    None
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so("test_driver", FIXTURE_C)
}

/// Compile an inline C source naming a driver nothing may be loaded as `t`.
fn compile_bad(stem: &str, source: &str) -> Option<String> {
    let so = compile_so(stem, source)?;
    so_path(&so)
}

/// Read the atom/tuple `{t, 1}` a `send_term` recorded, proving the term-data path built it.
fn assert_is_the_t_tuple(host: &TestDriverHost, term: beam_term::prelude::Eterm) {
    // SAFETY: `term` was built on `host`'s heap by `decode_term_spec` and the heap has not been
    // reset since, so it is a live 2-tuple.
    assert!(unsafe { is_tuple(term) }, "erl_drv_output_term must have built a tuple");
    // SAFETY: as above — a live tuple.
    assert_eq!(unsafe { tuple_size(term) }, 2, "{term:?} is not a 2-tuple");
    // SAFETY: element 0 of the live 2-tuple.
    let key = unsafe { tuple_element(term, 0) };
    // SAFETY: element 1 of the live 2-tuple.
    let value = unsafe { tuple_element(term, 1) };
    assert!(is_atom(key), "the tuple key must be the atom `t`");
    assert_eq!(host.atoms().name(atom_val(key)).as_deref(), Some("t"));
    assert!(is_small(value), "the tuple value must be the small int 1");
    assert_eq!(signed_value(value), 1);
}

// ---------------------------------------------------------------------------------------------
// The frozen `driver_*` table
// ---------------------------------------------------------------------------------------------

#[test]
fn the_driver_table_is_populated_for_every_symbol_the_fixture_imports() {
    // Calling `driver_api_table()` both pins the frozen table shape and forces the
    // `driver_*`-carrying codegen unit into the link, so `--export-dynamic` can publish it.
    let table = driver_api_table();
    assert!(table.driver_alloc.is_some());
    assert!(table.driver_free.is_some());
    assert!(table.driver_output.is_some());
    assert!(table.driver_set_timer.is_some());
    assert!(table.driver_cancel_timer.is_some());
    assert!(table.driver_select.is_some());
    assert!(table.driver_async.is_some());
    assert!(table.driver_mk_atom.is_some());
    assert!(table.driver_mk_port.is_some());
    assert!(table.erl_drv_output_term.is_some());
    assert!(table.driver_system_info.is_some());
    // The driver-registry / port-creation family is populated too (its slots and their C failure
    // values are exercised in `driver_lifecycle.rs` and in the family's own unit tests).
    assert!(table.driver_create_port.is_some());
    assert!(table.driver_lock_driver.is_some());
    assert!(table.add_driver_entry.is_some());
    assert!(table.remove_driver_entry.is_some());
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_driver_symbols_dynamically() {
    // Force the `driver_*` definitions into the link before inspecting it.
    assert!(driver_api_table().driver_alloc.is_some());

    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_driver_symbols: /proc/self/exe is unavailable");
        return;
    };
    match all_symbols_are_exported(&exe, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "`build.rs` emits `-Wl,--export-dynamic` for the test targets, so {} must appear in \
             {:?}'s dynamic symbol table",
            FIXTURE_IMPORTS.join("/"),
            exe
        ),
        None => skip("the_test_binary_exports_the_driver_symbols: no nm/readelf available"),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load a real driver `.so` and drive its callbacks
// ---------------------------------------------------------------------------------------------

#[test]
fn loads_and_drives_a_real_c_driver() {
    // Load-bearing: this pins the frozen `driver_*` table AND keeps the `driver_*`-carrying codegen
    // unit in the link (without it the `.so`'s imports would not resolve and `dlopen` would fail).
    assert!(driver_api_table().driver_alloc.is_some());

    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // --- the `.so` really imports `driver_*` (bound at load time, not at link time) -------------
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "the fixture must reference {} as undefined dynamic symbols; inspect {} with `nm -D`",
            FIXTURE_IMPORTS.join("/"),
            so.display()
        ),
        None => skip("loads_and_drives: no nm/readelf to inspect the .so's dynamic symbols"),
    }

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ----------------------------------------
    let lib = load(&path, "t")
        .expect("a real driver must load: its driver_* imports resolve from the test binary");

    // The validated entry fields (a shape check of the load contract).
    assert_eq!(lib.name(), "t");
    assert_eq!(lib.entry().extended_marker, ERL_DRV_EXTENDED_MARKER);
    assert_eq!(lib.entry().major_version, ERL_DRV_EXTENDED_MAJOR_VERSION);
    assert_eq!(lib.entry().minor_version, ERL_DRV_EXTENDED_MINOR_VERSION);

    // --- the port/state and the callbacks -------------------------------------------------------
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");

    // `start` allocates its private state and hands it back verbatim (never a failure code).
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null(), "start must return the private state, not a failure code");

    // `output` echoes through `driver_output(st->port, …)`: the bytes must land in the host.
    let hello = b"hello\0world";
    // SAFETY: `data` is the live state from `start`; `hello` is a readable buffer of `len` bytes.
    lib.output(data, hello.as_ptr() as *mut c_char, hello.len())
        .expect("the fixture implements output");
    assert_eq!(
        host.outputs(),
        &[hello.to_vec()],
        "driver_output must deliver the received bytes to the port owner"
    );

    // `control(1, …)` echoes at most `rlen` bytes into the reply buffer and returns that length.
    let payload = b"abcdef";
    let mut reply = vec![0u8; 4];
    let mut reply_ptr = reply.as_mut_ptr() as *mut c_char;
    // SAFETY: `data` is live; `payload` is readable for its length; `reply_ptr` points at 4 writable
    // bytes and `rlen` reports that capacity.
    let n = lib
        .control(
            data,
            1,
            payload.as_ptr() as *mut c_char,
            payload.len(),
            &mut reply_ptr,
            reply.len(),
        )
        .expect("the fixture implements control");
    assert_eq!(n, reply.len() as libc::ssize_t, "control must return min(len, rlen)");
    assert_eq!(&reply[..], &payload[..reply.len()], "control must echo the payload");

    // `control(0, …)` is the fixture's documented no-op.
    // SAFETY: `data` is live; the command touches no buffer for `command == 0`.
    let zero = lib
        .control(data, 0, std::ptr::null_mut(), 0, &mut reply_ptr, 0)
        .expect("the fixture implements control");
    assert_eq!(zero, 0);

    // `control(2, …)` schedules a timer through the host seam.
    // SAFETY: `data` is live; the timer command reads no buffer.
    assert_eq!(
        lib.control(data, 2, std::ptr::null_mut(), 0, &mut reply_ptr, 0)
            .expect("control"),
        0
    );
    assert_eq!(host.timers(), &[100u64 as libc::c_ulong]);

    // `control(3, …)` selects for read+use: the mode is the C header's `ERL_DRV_READ | ERL_DRV_USE`.
    // SAFETY: `data` is live; the select command reads no buffer.
    assert_eq!(
        lib.control(data, 3, std::ptr::null_mut(), 0, &mut reply_ptr, 0)
            .expect("control"),
        0
    );
    assert_eq!(host.selects(), &[(ERL_DRV_READ | ERL_DRV_USE, 1)]);

    // `control(4, …)` cancels the timer.
    // SAFETY: `data` is live; the cancel command reads no buffer.
    assert_eq!(
        lib.control(data, 4, std::ptr::null_mut(), 0, &mut reply_ptr, 0)
            .expect("control"),
        0
    );
    assert_eq!(host.cancels(), 1);

    // `control(5, …)` outputs the term-data 2-tuple `{t, 1}` (decode_term_spec + send_term).
    // SAFETY: `data` is live; the term command reads no buffer.
    assert_eq!(
        lib.control(data, 5, std::ptr::null_mut(), 0, &mut reply_ptr, 0)
            .expect("control"),
        0
    );
    let sent = host.sent();
    assert_eq!(sent.len(), 1, "erl_drv_output_term must deliver exactly one term");
    let (to, term) = sent[0];
    assert!(to.is_none(), "erl_drv_output_term targets the port owner, not a specific process");
    assert_is_the_t_tuple(&host, term);

    // `control(6, …)` fills an ErlDrvSysInfo and reports the driver ABI major version it carries.
    // SAFETY: `data` is live; the system-info command reads no buffer.
    let major = lib
        .control(data, 6, std::ptr::null_mut(), 0, &mut reply_ptr, 0)
        .expect("control");
    assert_eq!(major, ERL_DRV_EXTENDED_MAJOR_VERSION as libc::ssize_t);

    // `control(7, …)` schedules an async job through the host seam.
    // SAFETY: `data` is live; the async command reads no buffer.
    assert_eq!(
        lib.control(data, 7, std::ptr::null_mut(), 0, &mut reply_ptr, 0)
            .expect("control"),
        0
    );
    assert_eq!(host.asyncs(), 1);

    // `timeout` bumps the fixture's counter and re-outputs a marker through `driver_output`.
    // SAFETY: `data` is the live state from `start`.
    lib.timeout(data).expect("the fixture implements timeout");
    assert_eq!(
        host.outputs(),
        &[hello.to_vec(), b"T".to_vec()],
        "the timeout callback must emit its marker"
    );

    // `stop` frees the private state (no leak, no double free: it is called exactly once here).
    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");

    // Dropping the library runs the `finish` callback (absent here) then `dlclose`s the object.
    drop(lib);
}

// ---------------------------------------------------------------------------------------------
// The oracle's load-time validation, through the real loader
// ---------------------------------------------------------------------------------------------

#[test]
fn rejects_a_shared_object_without_driver_init() {
    const NO_INIT_C: &str = r#"
#include <erl_driver.h>

int beam_abi_not_a_driver_init(void) { return 0; }
"#;

    let Some(path) = compile_bad("driver_end_to_end_noinit", NO_INIT_C) else {
        return;
    };
    let err = load(&path, "t").expect_err("must be rejected");
    assert!(matches!(err, DriverLoadError::NoDriverInit(_)), "got {err:?}");
    assert_eq!(err.reason_atom(), "no_driver_init");
}

#[test]
fn rejects_a_driver_with_a_mismatched_name() {
    const WRONG_NAME_C: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry other_entry = {
    .driver_name = "other",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(t) { return &other_entry; }
"#;

    let Some(path) = compile_bad("driver_end_to_end_wrongname", WRONG_NAME_C) else {
        return;
    };
    let err = load(&path, "t").expect_err("must be rejected");
    assert!(matches!(err, DriverLoadError::BadDriverName(_)), "got {err:?}");
    assert_eq!(err.reason_atom(), "bad_driver_name");
}

#[test]
fn rejects_a_driver_that_needs_a_newer_runtime() {
    const FUTURE_C: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry futuristic_entry = {
    .driver_name = "t",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = 99,
    .minor_version = 0,
};

DRIVER_INIT(t) { return &futuristic_entry; }
"#;

    let Some(path) = compile_bad("driver_end_to_end_future", FUTURE_C) else {
        return;
    };
    let err = load(&path, "t").expect_err("must be rejected");
    assert!(matches!(err, DriverLoadError::DriverIncorrectVersion(_)), "got {err:?}");
    assert_eq!(err.reason_atom(), "driver_incorrect_version");
}

#[test]
fn a_missing_shared_object_is_a_typed_error_never_a_crash() {
    let path = format!("/tmp/beam_abi-driver-e2e-does-not-exist-{}.so", std::process::id());
    let err = load(&path, "t").expect_err("loading a nonexistent object must fail");
    assert_eq!(err.reason_atom(), "unspecified_error");
    assert!(
        err.text().contains("does-not-exist") || err.text().contains(&path),
        "the loader's description should name the path: {}",
        err.text()
    );
}
