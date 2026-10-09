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

//! The dynamic-library primitive ([`beam_io::ddll`]) against **real shared objects on disk**.
//!
//! `src/ddll.rs`'s in-module unit tests pin the pure pieces (the `errcode_tab` reproduction, the
//! atom/code/format-text tables, the flag bits) and the loader behaviour against a system library.
//! This suite is the **end-to-end** gate: it opens real objects with real `dlopen`, resolves real
//! symbols, calls one of them, and pins the three loader-failure shapes against objects built for
//! the purpose:
//!
//! * a **missing file** (nothing on disk) — the emulator's `{open_error, Code}` case;
//! * a **format error** (a file that exists but is not a loadable object);
//! * an **unresolved symbol** — both a `dlsym` miss on a loaded object, and a `dlopen(RTLD_NOW)`
//!   failure of an object whose reference nothing defines (`tests/fixtures/ddll_undefined.c`).
//!
//! # Skipping
//!
//! The fixture `.so`s need a C compiler and the system OTP include directory (the helpers in
//! `tests/support/mod.rs`, the same convention as the C-driver suites). A missing toolchain is
//! reported with a **loud** [`skip`] banner, never a silent pass; a fixture that fails to build
//! with a toolchain present is a hard assertion failure. The system-library cases skip loudly when
//! the platform exposes none of the usual sonames.
//!
//! # What is deliberately NOT asserted
//!
//! `badfile`, `undefined_symbol`, `not_a_driver` and `not_supported` are **not** `erl_ddll` error
//! atoms (`erl_bif_ddll.c`'s `errcode_tab` has no such rows, and the oracle's
//! `erl_ddll:format_error/1` raises `badarg` for each of them). The *situations* they name are
//! pinned here through the classification predicates (`is_missing_file`, `is_format_error`,
//! `is_undefined_symbol`) and the reason atoms the emulator actually renders — see
//! [`the_error_to_atom_mapping_is_pinned_to_the_emulators_vocabulary`].

mod support;

use std::ffi::CString;
use std::path::{Path, PathBuf};

use beam_io::ddll::{
    atom_to_errcode, errcode_to_atom, format_error_text, symbol_name, DllErrorKind, DynLib,
    DynLibFlags,
};

use support::{cc_available, compile_so, erl_include_dir, scratch_dir, skip};

/// Shared objects present on essentially every Linux/macOS host, in preference order.
const SYSTEM_LIBS: &[&str] = &["libc.so.6", "libm.so.6", "libdl.so.2", "libSystem.B.dylib"];

/// Open the first loadable system library; `None` (with a loud banner) when none loads.
fn open_system_lib() -> Option<DynLib> {
    for name in SYSTEM_LIBS {
        let c = CString::new(*name).expect("no NUL in a literal");
        if let Ok(lib) = DynLib::open_cstr(&c, DynLibFlags::EMULATOR) {
            return Some(lib);
        }
    }
    skip(&format!("none of {SYSTEM_LIBS:?} could be dlopen-ed on this host"));
    None
}

/// A committed fixture source under `tests/fixtures/`.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Compile `fixture(rel)` into a fresh scratch dir, returning the `.so` path — or `None` (loudly
/// skipped) when the C toolchain or the OTP include directory is unavailable. A build failure with
/// the toolchain present is a hard assertion failure.
fn build_fixture(rel: &str, stem: &str) -> Option<PathBuf> {
    if !cc_available() {
        skip(&format!("no C compiler (cc/$CC) for the {rel} fixture"));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip(&format!("no system OTP include directory (erl_nif.h) for the {rel} fixture"));
        return None;
    };
    let out = scratch_dir(stem).join(format!("{stem}.so"));
    assert!(compile_so(&fixture(rel), &out, &include), "the {rel} fixture must build");
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// The successful path
// ---------------------------------------------------------------------------------------------

#[test]
fn opens_a_system_library_and_resolves_a_known_symbol() {
    let Some(lib) = open_system_lib() else { return };
    // `malloc` (and `getpid`) are exported by libc/libm/libSystem on every supported host.
    let malloc = symbol_name("malloc").unwrap();
    let sym = lib.symbol(&malloc).expect("malloc must resolve");
    assert!(!sym.is_null(), "malloc is a real symbol");
    assert_eq!(sym, lib.try_symbol(&malloc).expect("try_symbol agrees with symbol"));
    // The handle is the same live `dlopen` handle the object was opened with.
    assert!(!lib.handle().is_null());
}

#[test]
fn a_dropped_handle_closes_cleanly_and_a_second_open_works() {
    let Some(first) = open_system_lib() else { return };
    let name = symbol_name("getpid").unwrap();
    assert!(first.symbol(&name).is_ok());
    assert!(!first.handle().is_null());
    drop(first);
    // A fresh open after the first handle was closed must succeed: no leaked loader state, no
    // double-close, no use-after-close.
    let Some(second) = open_system_lib() else {
        panic!("a second open of a library that opened once must work");
    };
    assert!(second.symbol(&name).is_ok());
}

#[test]
fn a_fixture_object_opens_resolves_and_can_be_called() {
    let Some(path) = build_fixture("ddll_symbol.c", "ddll-symbol") else { return };
    let lib = DynLib::open(&path).expect("the fixture must dlopen");

    // A data symbol and a function symbol both resolve.
    let value = lib.symbol(&symbol_name("beam_io_ddll_fixture_value").unwrap()).unwrap();
    assert!(!value.is_null());
    let double = lib.symbol(&symbol_name("beam_io_ddll_fixture_double").unwrap()).unwrap();
    assert!(!double.is_null());

    // Calling through the resolved address proves the symbol really is the fixture's function.
    // SAFETY: the fixture exports `int beam_io_ddll_fixture_double(int)`, compiled by the system
    // `cc` in this test's own scratch directory, and `double` is its resolved address (checked
    // non-null above). The signature is transcribed exactly from the fixture source.
    let double_fn: extern "C" fn(i32) -> i32 = unsafe { std::mem::transmute(double) };
    assert_eq!(double_fn(21), 42);

    // A name the fixture does not carry is a typed miss, not a null pointer.
    let err = lib
        .symbol(&symbol_name("beam_io_ddll_fixture_absent").unwrap())
        .expect_err("the fixture has no such symbol");
    assert_eq!(err.kind(), DllErrorKind::SymbolNotFound);
    assert_eq!(err.reason_atom(), "symbol_not_found");
    assert!(err.is_undefined_symbol());
}

// ---------------------------------------------------------------------------------------------
// The three loader-failure shapes
// ---------------------------------------------------------------------------------------------

#[test]
fn a_missing_file_is_the_file_not_found_shape() {
    let path = Path::new("/nonexistent/beam_io-ddll-integration-missing.so");
    let err = DynLib::open(path).expect_err("a nonexistent object must not open");
    assert_eq!(err.kind(), DllErrorKind::Open);
    assert_eq!(err.reason_atom(), "open_error", "the emulator renders {{open_error, Code}}");
    assert!(err.is_missing_file(), "{err}");
    assert!(!err.is_format_error());
    assert!(!err.is_undefined_symbol());
    assert!(err.text().contains("No such file or directory"), "{err}");
}

#[test]
fn a_text_file_is_a_format_error() {
    let dir = scratch_dir("ddll-notelf");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("not-an-object.so");
    std::fs::write(&path, b"this is plain text, not an ELF object\n").expect("write fixture");

    let err = DynLib::open(&path).expect_err("a text file is not a loadable object");
    assert_eq!(err.kind(), DllErrorKind::Open);
    assert_eq!(err.reason_atom(), "open_error");
    assert!(
        err.is_format_error(),
        "glibc calls this a format error ('file too short'/'invalid ELF header'): {err}"
    );
    assert!(!err.is_missing_file(), "the file exists: {err}");
}

#[test]
fn an_undefined_symbol_in_a_loaded_object_fails_the_load() {
    let Some(path) = build_fixture("ddll_undefined.c", "ddll-undefined") else { return };
    // `RTLD_NOW` resolves every undefined symbol eagerly, and nothing defines the fixture's probe.
    let err = DynLib::open(&path)
        .err()
        .unwrap_or_else(|| panic!("RTLD_NOW must reject an object with an unresolved symbol"));
    assert_eq!(err.kind(), DllErrorKind::Open);
    assert_eq!(err.reason_atom(), "open_error");
    assert!(
        err.is_undefined_symbol(),
        "the loader text must name the unresolved symbol: {err}"
    );
    assert!(err.text().contains("beam_io_ddll_undefined_probe"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The error -> atom mapping
// ---------------------------------------------------------------------------------------------

#[test]
fn the_error_to_atom_mapping_is_pinned_to_the_emulators_vocabulary() {
    // --- missing file: the emulator emits `{open_error, Code}` (there is no `badfile` row in
    //     `errcode_tab`, and `erl_ddll:format_error(badfile)` raises badarg). ------------------
    let missing = DynLib::open(Path::new("/nonexistent/beam_io-ddll-atom-map.so"))
        .expect_err("missing file");
    assert_eq!(missing.reason_atom(), "open_error");
    assert!(missing.is_missing_file());

    // --- format error: the same `open_error` case, recognised by its text. -------------------
    let dir = scratch_dir("ddll-atom-map");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let text_file = dir.join("empty.so");
    std::fs::write(&text_file, b"").expect("write fixture");
    let format = DynLib::open(&text_file).expect_err("empty file");
    assert_eq!(format.reason_atom(), "open_error");
    assert!(format.is_format_error(), "{format}");

    // --- undefined symbol: a `dlsym` miss is the table atom `symbol_not_found`. ---------------
    let Some(lib) = open_system_lib() else { return };
    let miss = lib
        .symbol(&symbol_name("beam_io_ddll_no_such_symbol_probe").unwrap())
        .expect_err("a bogus symbol must not resolve");
    assert_eq!(miss.reason_atom(), "symbol_not_found");
    assert!(miss.is_undefined_symbol());
    assert_eq!(
        format_error_text(miss.reason_atom()),
        Some("Symbol not found in dynamic library"),
        "the format text matches erts_ddll_error"
    );

    // --- the table itself, exactly as `erl_bif_ddll.c:1709-1720`. ------------------------------
    assert_eq!(errcode_to_atom(0), Some("no_error"));
    assert_eq!(errcode_to_atom(-1), Some("no_driver_init"));
    assert_eq!(errcode_to_atom(-2), Some("driver_init_failed"));
    assert_eq!(errcode_to_atom(-5), Some("driver_incorrect_version"));
    assert_eq!(errcode_to_atom(-8), Some("symbol_not_found"));
    assert_eq!(errcode_to_atom(-10), None, "a dynamic open_error code has no table atom");
    assert_eq!(atom_to_errcode("no_driver_init"), Some(-1));
    assert_eq!(format_error_text("unspecified_error"), Some("Unspecified dynamic library error"));

    // --- the atoms the emulator does NOT know: they map to nothing, so a floor must not use
    //     them for `{error, Atom}`. -------------------------------------------------------------
    for unknown in ["badfile", "undefined_symbol", "not_a_driver", "not_supported"] {
        assert_eq!(atom_to_errcode(unknown), None, "{unknown} is not an erl_ddll atom");
        assert_eq!(format_error_text(unknown), None, "{unknown} has no erts_ddll_error text");
    }
}

#[test]
fn the_default_mode_is_the_emulators_and_global_scope_is_opt_in() {
    // The emulator opens drivers/NIFs with `RTLD_NOW | RTLD_LOCAL` (`erl_unix_sys_ddll.c:133`).
    assert_eq!(DynLibFlags::default().bits(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    assert_eq!(DynLibFlags::EMULATOR.bits(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    assert_eq!(DynLibFlags::NOW_GLOBAL.bits(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
    // And a library opened with the default mode loads and resolves fine.
    let Some(lib) = open_system_lib() else { return };
    assert!(lib.symbol(&symbol_name("malloc").unwrap()).is_ok());
}
