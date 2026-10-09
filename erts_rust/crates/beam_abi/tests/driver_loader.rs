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

//! The dynamic **driver** loader (`beam_abi::driver::loader::load`) — the `ErlDrvEntry` load
//! contract's error taxonomy and happy path (workspace contract **A.4**).
//!
//! `load(path, name)` reproduces `do_load_driver_entry` (`erts/emulator/beam/erl_bif_ddll.c:1466-1533`),
//! `erts_sys_ddll_load_driver_init` (`erts/emulator/sys/unix/erl_unix_sys_ddll.c:216-228`) and
//! `init_driver` (`erts/emulator/beam/io.c:7559-7621`). Every failure is a typed
//! [`DriverLoadError`] carrying the oracle's `errcode_tab` reason atom
//! (`erl_bif_ddll.c:1709-1720`) and its `erts_ddll_error` text (`erl_bif_ddll.c:1201-1224`).
//!
//! Each fixture is a **real shared object** compiled at test time with the system `cc` against the
//! system `erl_driver.h` and `dlopen`ed by the loader. The error fixtures declare their
//! `ErlDrvEntry` inline (like `tests/loader.rs`'s `custom_entry_source`); the happy-path fixture is
//! the committed `tests/fixtures/driver_loader_finish.c`, whose `finish` callback records that
//! `DriverLibrary::drop` ran it before `dlclose`.
//!
//! # Skip-vs-fail policy
//!
//! Building a fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`). When either is missing [`compile_so`] prints a loud [`skip`] banner and the test
//! returns — a skip is never a silent pass. When the toolchain *is* present, every assertion is
//! hard.

mod common;

use std::path::{Path, PathBuf};

use beam_abi::driver::loader::{load, DriverCallError, DriverLoadError};
use beam_abi::driver::types::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MARKER, ERL_DRV_EXTENDED_MINOR_VERSION,
};

use common::{compile_so, skip};

/// The committed happy-path fixture (see the file header for the `finish`-recording rationale).
const FINISH_FIXTURE_C: &str = include_str!("fixtures/driver_loader_finish.c");

// ---------------------------------------------------------------------------------------------
// Fixture sources (inline; each is a real driver `.so` compiled at test time)
// ---------------------------------------------------------------------------------------------

/// A shared object with an unrelated symbol and **no** `driver_init`.
const NO_INIT: &str = r#"
#include <erl_driver.h>

int beam_abi_loader_not_a_driver_init(void) { return 0; }
"#;

/// `driver_init()` returns `NULL`.
const NULL_INIT: &str = r#"
#include <erl_driver.h>

DRIVER_INIT(t) { return NULL; }
"#;

/// An entry with **no** `ERL_DRV_EXTENDED_MARKER` (the older-than-extended case).
const NO_MARKER: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry t_entry = {
    .driver_name = "t",
    .extended_marker = 0,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(t) { return &t_entry; }
"#;

/// A well-marked entry whose `major_version` is too old for this runtime.
const TOO_OLD: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry t_entry = {
    .driver_name = "t",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = 1,
    .minor_version = 0,
};

DRIVER_INIT(t) { return &t_entry; }
"#;

/// A well-marked entry whose `major_version` needs a newer runtime.
const TOO_NEW: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry t_entry = {
    .driver_name = "t",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = 99,
    .minor_version = 0,
};

DRIVER_INIT(t) { return &t_entry; }
"#;

/// A well-marked entry whose `driver_name` does not match the requested name.
const WRONG_NAME: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry t_entry = {
    .driver_name = "other",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(t) { return &t_entry; }
"#;

/// A well-marked entry whose `driver_name` is `NULL` (the C would fault in `sys_strcmp`).
const NULL_NAME: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry t_entry = {
    .driver_name = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(t) { return &t_entry; }
"#;

/// A fully valid entry whose `init` callback vetoes the load with a non-zero result.
const VETO_INIT: &str = r#"
#include <erl_driver.h>

static int veto_init(void) { return 7; }

static ErlDrvEntry t_entry = {
    .init = veto_init,
    .driver_name = "t",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(t) { return &t_entry; }
"#;

/// A valid entry exported through the **`_driver_init`** fallback symbol only (no `driver_init`).
const UNDERSCORE_INIT: &str = r#"
#include <erl_driver.h>

static ErlDrvEntry t_entry = {
    .driver_name = "t",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

ErlDrvEntry* _driver_init(void) { return &t_entry; }
"#;

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

/// Compile an inline fixture and return its `.so` path; `None` (after a loud skip) when the
/// toolchain is absent.
fn compile(stem: &str, source: &str) -> Option<String> {
    let so = compile_so(stem, source)?;
    so_path(&so)
}

/// Load `path`, require the oracle's `Reason` reason-atom `atom` and its text, and return the error.
fn expect_reason<T: std::fmt::Debug>(
    result: Result<T, DriverLoadError>,
    atom: &str,
    text: &str,
) -> DriverLoadError {
    let err = result.expect_err("the load must be rejected");
    assert_eq!(err.reason_atom(), atom, "unexpected reason atom for {err:?}");
    assert_eq!(err.text(), text, "unexpected diagnostic text for {err:?}");
    err
}

// ---------------------------------------------------------------------------------------------
// The oracle's load-time validation order — every `errcode_tab` reason atom
// ---------------------------------------------------------------------------------------------

#[test]
fn a_shared_object_without_driver_init_is_no_driver_init() {
    let Some(path) = compile("driver_loader_noinit", NO_INIT) else {
        return;
    };
    let err = expect_reason(
        load(&path, "t"),
        "no_driver_init",
        "No driver init in dynamic library",
    );
    assert!(matches!(err, DriverLoadError::NoDriverInit(_)), "got {err:?}");
}

#[test]
fn driver_init_returning_null_is_driver_init_failed() {
    let Some(path) = compile("driver_loader_nullinit", NULL_INIT) else {
        return;
    };
    let err = expect_reason(load(&path, "t"), "driver_init_failed", "Driver init failed");
    assert!(matches!(err, DriverLoadError::DriverInitFailed(_)), "got {err:?}");
}

#[test]
fn a_missing_extended_marker_is_driver_incorrect_version() {
    let Some(path) = compile("driver_loader_nomarker", NO_MARKER) else {
        return;
    };
    let err = expect_reason(
        load(&path, "t"),
        "driver_incorrect_version",
        "Driver compiled with incorrect version of erl_driver.h",
    );
    assert!(matches!(err, DriverLoadError::DriverIncorrectVersion(_)), "got {err:?}");
}

#[test]
fn an_old_major_version_is_driver_incorrect_version() {
    let Some(path) = compile("driver_loader_toolold", TOO_OLD) else {
        return;
    };
    let err = expect_reason(
        load(&path, "t"),
        "driver_incorrect_version",
        "Driver compiled with incorrect version of erl_driver.h",
    );
    assert!(matches!(err, DriverLoadError::DriverIncorrectVersion(_)), "got {err:?}");
}

#[test]
fn a_future_major_version_is_driver_incorrect_version() {
    let Some(path) = compile("driver_loader_toonew", TOO_NEW) else {
        return;
    };
    let err = expect_reason(
        load(&path, "t"),
        "driver_incorrect_version",
        "Driver compiled with incorrect version of erl_driver.h",
    );
    assert!(matches!(err, DriverLoadError::DriverIncorrectVersion(_)), "got {err:?}");
}

#[test]
fn a_mismatched_driver_name_is_bad_driver_name() {
    let Some(path) = compile("driver_loader_wrongname", WRONG_NAME) else {
        return;
    };
    let err = expect_reason(load(&path, "t"), "bad_driver_name", "Bad driver name");
    assert!(matches!(err, DriverLoadError::BadDriverName(_)), "got {err:?}");
}

#[test]
fn a_null_driver_name_is_bad_driver_name() {
    let Some(path) = compile("driver_loader_nullname", NULL_NAME) else {
        return;
    };
    let err = expect_reason(load(&path, "t"), "bad_driver_name", "Bad driver name");
    assert!(matches!(err, DriverLoadError::BadDriverName(_)), "got {err:?}");
}

#[test]
fn a_vetoing_init_callback_is_driver_init_failed() {
    let Some(path) = compile("driver_loader_veto", VETO_INIT) else {
        return;
    };
    let err = expect_reason(load(&path, "t"), "driver_init_failed", "Driver init failed");
    assert!(matches!(err, DriverLoadError::DriverInitFailed(_)), "got {err:?}");
}

#[test]
fn a_missing_shared_object_is_unspecified() {
    // No toolchain needed: this path never opens.
    let missing = std::env::temp_dir().join(format!(
        "beam_abi-driver-loader-does-not-exist-{}.so",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&missing);
    let path = missing.to_str().expect("the temp path is UTF-8");

    let err = load(path, "t").expect_err("a nonexistent object must not load");
    assert!(matches!(err, DriverLoadError::Unspecified(_)), "got {err:?}");
    assert_eq!(err.reason_atom(), "unspecified_error");
    // `dlerror` names the file we tried to open.
    assert!(
        err.text().contains(path) || err.text().contains("does-not-exist"),
        "the loader's description should name the path: {}",
        err.text()
    );
}

// ---------------------------------------------------------------------------------------------
// The `_driver_init` fallback symbol
// ---------------------------------------------------------------------------------------------

#[test]
fn the_underscore_driver_init_fallback_symbol_is_accepted() {
    let Some(path) = compile("driver_loader_underscore", UNDERSCORE_INIT) else {
        return;
    };
    // The object exports `_driver_init` only; the loader's fallback lookup must find it, and the
    // ordinary validation then applies (this entry is valid).
    let lib = load(&path, "t").expect("the `_driver_init` fallback must resolve");
    assert_eq!(lib.name(), "t");
    assert_eq!(lib.entry().extended_marker, ERL_DRV_EXTENDED_MARKER);
}

// ---------------------------------------------------------------------------------------------
// Happy path: a valid entry, the `.so` suffix rule, a callback wrapper, and `Drop`
// ---------------------------------------------------------------------------------------------

/// The environment variable the committed fixture reads to learn where to record `finish`.
const FINISH_MARKER_ENV: &str = "BEAM_ABI_DRIVER_LOADER_FINISH";

/// A fresh, non-existent scratch path for the `finish` marker.
fn finish_marker_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "beam_abi-driver-loader-finish-{}-{}.txt",
        std::process::id(),
        // A distinct name per call keeps repeated runs on one process (e.g. `--test-threads=1`
        // reruns) from reading a stale marker.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

#[test]
fn loads_a_valid_driver_appends_the_suffix_and_runs_finish_on_drop() {
    let Some(so) = compile_so("driver_loader_finish", FINISH_FIXTURE_C) else {
        return;
    };
    let Some(full) = so_path(&so) else { return };
    // Strip the suffix so `load` has to append it (the documented `FILE_EXT` behaviour): the lone
    // file on disk is `<stem>.so`, so a successful load proves the suffix was appended.
    let stem = full
        .strip_suffix(".so")
        .expect("`compile_so` always writes a `.so`")
        .to_string();

    // Point the fixture's `finish` callback at a fresh marker file.
    let marker = finish_marker_path();
    let _ = std::fs::remove_file(&marker);
    std::env::set_var(FINISH_MARKER_ENV, &marker);

    let lib = load(&stem, "t").expect("a valid driver must load through the appended suffix");

    // --- name() / entry() read-back (the load contract's shape) ---------------------------------
    assert_eq!(lib.name(), "t");
    assert_eq!(lib.entry().extended_marker, ERL_DRV_EXTENDED_MARKER);
    assert_eq!(lib.entry().major_version, ERL_DRV_EXTENDED_MAJOR_VERSION);
    assert_eq!(lib.entry().minor_version, ERL_DRV_EXTENDED_MINOR_VERSION);
    assert!(lib.init().is_none(), "the fixture's entry has no `init` callback");

    // --- a callback wrapper really drives the C code --------------------------------------------
    // `control(cmd)` returns `command + 1` from inside the loaded object.
    let echoed = lib
        .control(std::ptr::null_mut(), 5, std::ptr::null_mut(), 0, std::ptr::null_mut(), 0)
        .expect("the fixture implements control");
    assert_eq!(echoed, 6);

    // A slot the fixture leaves NULL is reported as an absent callback, not silently ignored.
    match lib.output(std::ptr::null_mut(), std::ptr::null_mut(), 0) {
        Err(absent) => {
            assert!(absent.is_absent());
            assert_eq!(absent, DriverCallError::AbsentCallback { callback: "output" });
        }
        Ok(()) => panic!("the fixture has no `output` callback"),
    }

    // --- `finish` has not run yet ----------------------------------------------------------------
    assert!(!marker.exists(), "`finish` must run only on drop, not at load");

    // --- Drop runs `finish` (then `dlclose`) ------------------------------------------------------
    drop(lib);
    let recorded = std::fs::read_to_string(&marker).expect("`Drop` must have run the `finish` callback");
    assert!(
        recorded.contains("finish"),
        "the `finish` callback must have recorded its run: {recorded:?}"
    );
    let _ = std::fs::remove_file(&marker);
    std::env::remove_var(FINISH_MARKER_ENV);
}
