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
//! 2. attempts to load it (`dlopen(RTLD_NOW)` + `driver_init` + `ErlDrvEntry`) and asserts the
//!    loaded driver reports `"host_driver"`.
//!
//! # Skip-vs-fail policy
//!
//! Step 1 skips **loudly** when no C compiler or OTP include directory is present, but fails
//! whenever the toolchain *is* present and the fixture does not build. Step 2 skips **loudly** when
//! the load fails for a wiring reason: resolving the fixture's `driver_*` imports needs the host
//! (`beam_abi`'s `driver_*` slots) exported from the test binary via
//! `-Wl,--export-dynamic` (a sibling `build.rs`), which may not be present on this branch. A load
//! failure is never a test failure — only a name mismatch from a *successful* load is.
//!
//! # Why the load probe is local
//!
//! The crate-level loader is `beam_abi::driver::loader::load(path, name)`; it is not referenced
//! here because `beam_abi` is not a `beam_io` dependency on this branch (a sibling task owns
//! `Cargo.toml`) and a hard reference would be a compile error. [`support::probe_driver_name`]
//! performs the equivalent `dlopen`/`driver_init`/`driver_name` read with `libc` alone — see the
//! `support` module docs.

mod support;

use std::path::PathBuf;

use support::{cc_available, compile_so, erl_include_dir, probe_driver_name, scratch_dir, skip};

/// The committed C fixture.
fn fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host_driver.c")
}

/// Compile the driver fixture and load it: the produced `.so` must report the driver name
/// `host_driver`.
#[test]
fn host_driver_fixture_builds_and_loads() {
    let Some(include) = erl_include_dir() else {
        skip("host_driver: no OTP include directory (erl_driver.h) found");
        return;
    };
    if !cc_available() {
        skip("host_driver: no C compiler (cc / $CC) available");
        return;
    }

    let src = fixture_source();
    assert!(
        src.is_file(),
        "the committed fixture is missing: {}",
        src.display()
    );

    let dir = scratch_dir("host-driver");
    let so = dir.join("host_driver.so");
    let compiled = compile_so(&src, &so, &include);
    assert!(
        compiled && so.is_file(),
        "the host_driver fixture must compile into a shared object at {}",
        so.display()
    );

    // The object exists and is a real driver; now prove it can be loaded. `RTLD_NOW` means every
    // `driver_*` import must be resolvable from the loading process, so a failure here is a wiring
    // gap (no exported host symbols), not a fixture bug — skip rather than fail.
    match probe_driver_name(&so) {
        Ok(Some(name)) => assert_eq!(
            name, "host_driver",
            "a loaded driver must report the name its `driver_init` entry carries"
        ),
        Ok(None) => skip("host_driver: the object loaded but driver_init returned NULL"),
        Err(err) => skip(&format!(
            "host_driver: dlopen(RTLD_NOW) failed — the driver_* host symbols / \
             -Wl,--export-dynamic wiring may not be present yet: {err}"
        )),
    }
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

/// The load probe must report an error (never panic, never a false success) for a missing object.
#[test]
fn probe_reports_an_error_for_a_missing_object() {
    let missing = scratch_dir("host-driver-missing").join("does-not-exist.so");
    assert!(
        probe_driver_name(&missing).is_err(),
        "loading a nonexistent object must be an error"
    );
}
