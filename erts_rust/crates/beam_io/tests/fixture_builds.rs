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

//! Builds the committed C driver fixture and proves it is a real, *loadable* driver shared object.
//!
//! `tests/fixtures/host_driver.c` is an ordinary driver: it `#include`s `erl_driver.h` and calls
//! `driver_alloc`, `driver_output`, `driver_select`, `driver_set_timer`, `erl_drv_output_term`, …
//! Its `driver_*` imports are left **undefined** by the linker, exactly like a real OTP driver, so
//! they can only be resolved out of the *loading process*'s dynamic symbol table. This test does
//! two things:
//!
//! 1. compiles the fixture with the system `cc` against the system `erl_driver.h` and asserts a
//!    shared object was produced (the fixture must stay buildable — a broken fixture is a hard
//!    failure, not a skip); and
//! 2. loads it with the crate-level loader `beam_abi::driver::loader::load` and asserts the loaded
//!    driver reports `"host_driver"`.
//!
//! # Skip-vs-fail policy
//!
//! Step 1 skips **loudly** when no C compiler or OTP include directory is present, but fails
//! whenever the toolchain *is* present and the fixture does not build. Step 2 is a **hard failure**
//! too: resolving the fixture's `driver_*` imports needs the host (`beam_abi`'s `driver_*` slots)
//! exported from the test binary via `-Wl,--export-dynamic` (a sibling `build.rs`) **and** a
//! reference that pulls `beam_abi`'s `drv` object file into the link — see
//! [`force_driver_symbols_link`]. A load failure therefore means the wiring is broken (`dlopen`
//! reports `undefined symbol: driver_mk_port`), which is exactly the kind of regression this test
//! must catch, not skip over.

mod support;

use std::path::PathBuf;

use support::{cc_available, compile_so, erl_include_dir, probe_driver_name, scratch_dir, skip};

/// The committed C fixture.
fn fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host_driver.c")
}

/// The committed `ready_async`-less variant fixture (`#include`s `host_driver.c` with the entry's
/// `ready_async` slot forced to `NULL`), used to exercise the host's `async_free` arm.
fn variant_fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host_driver_no_ready_async.c")
}

/// Force the `#[no_mangle]` `driver_*` host symbols into this test binary's **dynamic** symbol
/// table (see `tests/abi_driver.rs`, which carries the long explanation).
///
/// Naming [`beam_abi::driver::drv::driver_api_table`] — the `#[used]` table holding them — is what
/// pulls `beam_abi`'s `drv` object file into the link; `build.rs`'s `-Wl,--export-dynamic` then
/// puts them in `.dynsym` so the fixture's `dlopen(RTLD_NOW)` can bind them.
fn force_driver_symbols_link() {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
}

/// Compile `src` (stem `stem`) into a scratch `.so` and load it with the crate-level loader: the
/// produced object must be a real driver reporting the name `host_driver`. Skips **loudly** when no
/// C compiler or OTP include directory is present; a build or load failure with the toolchain
/// present is a hard failure (never a skip).
fn build_and_load(src: PathBuf, stem: &str) {
    let Some(include) = erl_include_dir() else {
        skip("host_driver: no OTP include directory (erl_driver.h) found");
        return;
    };
    if !cc_available() {
        skip("host_driver: no C compiler (cc / $CC) available");
        return;
    }

    assert!(
        src.is_file(),
        "the committed fixture is missing: {}",
        src.display()
    );

    let dir = scratch_dir(stem);
    let so = dir.join(format!("{stem}.so"));
    let compiled = compile_so(&src, &so, &include);
    assert!(
        compiled && so.is_file(),
        "the {stem} fixture must compile into a shared object at {}",
        so.display()
    );

    // The object exists and is a real driver; now prove it can be loaded. `RTLD_NOW` means every
    // `driver_*` import must be resolvable from the loading process, so a failure here is a wiring
    // gap (the host symbols are not exported), never a fixture bug — and never something to skip.
    force_driver_symbols_link();
    let path = so.to_str().expect("the fixture path is valid UTF-8");
    match beam_abi::driver::loader::load(path, "host_driver") {
        Ok(library) => assert_eq!(
            library.name(),
            "host_driver",
            "a loaded driver must report the name its `driver_init` entry carries"
        ),
        Err(err) => panic!(
            "the {stem} fixture could not be loaded with dlopen(RTLD_NOW): {err}\n\
             an `undefined symbol: driver_*` here means the driver_* host symbols are not in this \
             test binary's dynamic symbol table — check build.rs's -Wl,--export-dynamic and the \
             driver_api_table() reference in force_driver_symbols_link()."
        ),
    }
}

/// Compile the driver fixture and load it: the produced `.so` must report the driver name
/// `host_driver`.
#[test]
fn host_driver_fixture_builds_and_loads() {
    build_and_load(fixture_source(), "host_driver");
}

/// The same for the variant fixture (`ready_async` forced to `NULL`): it must stay buildable and
/// loadable too, so `tests/abi_async.rs`'s `async_free`-arm test can rely on it.
#[test]
fn host_driver_no_ready_async_variant_builds_and_loads() {
    build_and_load(variant_fixture_source(), "host_driver_no_ready_async");
}
/// The include-directory helper must return a directory that really holds the driver header (the
/// contract the fixture build relies on).
#[test]
fn the_driver_header_is_locatable() {
    match erl_include_dir() {
        Some(dir) => assert!(
            dir.join("erl_driver.h").is_file(),
            "erl_include_dir() must return a directory holding erl_driver.h, got {}",
            dir.display()
        ),
        None => skip("no OTP include directory (erl_driver.h) found on this host"),
    }
}

/// `compile_so` must report failure — and produce no artifact — for a source that is not valid C.
#[test]
fn compile_so_reports_failure_for_a_broken_source() {
    let Some(include) = erl_include_dir() else {
        skip("compile_so: no OTP include directory (erl_driver.h) found");
        return;
    };
    if !cc_available() {
        skip("compile_so: no C compiler (cc / $CC) available");
        return;
    }

    let dir = scratch_dir("host-driver-broken");
    std::fs::create_dir_all(&dir).expect("a scratch dir is creatable");
    let src = dir.join("broken.c");
    std::fs::write(&src, "#include <erl_driver.h>\nthis is not valid C at all;\n")
        .expect("the broken source is writable");
    let so = dir.join("broken.so");

    assert!(
        !compile_so(&src, &so, &include),
        "a non-compilable source must be reported as a failure"
    );
    assert!(
        !so.is_file(),
        "no shared object may be produced for a non-compilable source"
    );
}

/// The libc-only load probe must report an error (never panic, never a false success) for a missing
/// object.
///
/// `support::probe_driver_name` is the dependency-free `dlopen` check kept alongside the real
/// loader: it exposes the dynamic loader's own error text (which `beam_abi`'s typed
/// `DriverLoadError` keeps too, see the load assertion above), so the "a missing object must be an
/// error" contract is pinned at the rawest level as well.
#[test]
fn probe_reports_an_error_for_a_missing_object() {
    let missing = scratch_dir("host-driver-missing").join("does-not-exist.so");
    assert!(
        probe_driver_name(&missing).is_err(),
        "loading a nonexistent object must be an error"
    );
}
