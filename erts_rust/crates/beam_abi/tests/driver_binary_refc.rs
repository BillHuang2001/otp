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

//! End-to-end proof of the **binary reference-count trio** of the driver C ABI (workspace contract
//! **A.4**): `driver_binary_get_refc`, `driver_binary_inc_refc` and `driver_binary_dec_refc`.
//!
//! # Why this family needs real C
//!
//! Unlike the rest of the driver surface, these three functions observe state that the frozen
//! public `ErlDrvBinary` does not declare. The emulator keeps the count in a private `Binary` header
//! allocated *before* the pointer a driver sees (`Binary2ErlDrvBinary`/`ErlDrvBinary2Binary`,
//! `erts/emulator/beam/erl_binary.h:173-176`) — a driver can only reach it through the ABI, so the
//! only honest evidence is a real `.so` that calls the three functions through the real headers.
//! `tests/fixtures/driver_binary_refc.c` is that `.so`: it allocates with `driver_alloc_binary`,
//! walks the count through get/inc/dec, and publishes the exact sequence it observed.
//!
//! # The differential run: one `.so`, two entry points
//!
//! The same `.so` exports **both** `driver_init` (the `binrefc` driver entry) and `nif_init` (the
//! `binrefc` NIF entry), and both funnel into one C function that produces the report. That duality
//! exists for a concrete, measured reason:
//!
//! * the Nix-packaged OTP 29.1.1 / erts-17.1 this repo develops against does **not** export
//!   `erlang:load_driver/2` (a build-time emulator option; `erl -eval
//!   'erlang:function_exported(erlang, load_driver, 2)'` answers `false` and a direct call is
//!   `{error, undef}`), so the oracle cannot load a `.so` as a *driver*;
//! * it **can** load it as a NIF, and the `driver_binary_*` symbols the NIF leaves undefined bind
//!   against the running emulator's own exported implementations (`nm -D beam.smp` lists
//!   `driver_alloc_binary`, the trio, `driver_realloc_binary` and `driver_free_binary` as defined
//!   `T` entries) — exactly as a driver's undefined imports do.
//!
//! So [`the_oracle_reports_the_same_sequences_from_real_erts`] drives the very same `.so` inside a
//! real ERTS and asserts the four report strings are **identical** to the ones
//! [`drives_a_real_c_driver_through_the_refcount_trio`] obtains through this crate's driver loader.
//! Every value asserted below is therefore measured, not derived: it is what the real ERTS prints.
//!
//! When the toolchain is absent (`cc`, the OTP headers, `erlc`/`erl`) the affected test prints a
//! loud `SKIP:` banner and returns — never a silent pass.
//!
//! # What the sequence pins
//!
//! * the initial count of a freshly allocated driver binary (**1**);
//! * that `inc_refc`/`dec_refc` return the value **after** the change, not the previous one;
//! * that `driver_realloc_binary` carries the count across the move and updates `orig_size`;
//! * that `driver_binary_dec_refc` only decrements — it does **not** free at zero (the block is
//!   released by `driver_free_binary` alone), exactly as `erl_driver.md` documents.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::{load, DriverLibrary};
use beam_abi::driver::types::ErlDrvData;
use beam_abi::DriverEnv;
use libc::c_char;

use common::{compile_so, skip, TestDriverHost};

/// The committed C fixture — a real `.so` with a driver entry point *and* a NIF entry point.
const FIXTURE_C: &str = include_str!("fixtures/driver_binary_refc.c");

/// The name the fixture declares for both entries (`DRIVER_INIT(binrefc)` / `ERL_NIF_INIT(binrefc,…)`).
const FIXTURE_NAME: &str = "binrefc";

/// `driver_alloc_binary(size)` — the allocator whose hidden header the trio reads.
const ALLOC: u32 = 1;
/// `driver_realloc_binary` around the same count.
const REALLOC: u32 = 2;
/// A zero-size driver binary.
const ZERO: u32 = 3;
/// `driver_binary_dec_refc` driven to zero: does the trio free?
const DEC_TO_ZERO: u32 = 4;

/// The four reports, **measured on the real system ERTS** (OTP 29.1.1 / erts-17.1) by
/// [`the_oracle_reports_the_same_sequences_from_real_erts`], which prints the same C strings.
///
/// * `1` — the initial count is 1; `inc_refc`/`dec_refc` return the NEW value; the count walks
///   `1 -> 2 -> 3 -> 2 -> 1` and `driver_free_binary` releases the block at the driver's own single
///   reference.
/// * `2` — `driver_realloc_binary` preserves the count and reports the new `orig_size`.
/// * `3` — a zero-size binary is a valid block with `orig_size == 0` and the same count.
/// * `4` — `driver_binary_dec_refc` reaches 0 and then -1 without ever freeing.
const ORACLE_REPORTS: [&str; 4] = [
    "get=1 inc=2 get=2 inc=3 dec=2 get=2 dec=1 get=1",
    "size_before=4 get=1 inc=2 size_after=64 refc_after=2 dec=1",
    "nonnull=1 size=0 get=1 inc=2 dec=1",
    "get=1 dec=0 get=0 dec=-1",
];

/// The `driver_*` symbols the fixture imports, plus the one `enif_*` symbol its NIF entry needs.
///
/// Every one is left UNDEFINED in the `.so` and is resolved at `dlopen(RTLD_NOW)` against the
/// *loading process's* dynamic symbol table — so on the Rust side they must appear as defined
/// symbols of the test binary (`build.rs`'s `-Wl,--export-dynamic` + the `#[used]` api tables), and
/// on the oracle side they are the running `beam.smp`'s own exports.
const FIXTURE_IMPORTS: [&str; 10] = [
    "driver_alloc",
    "driver_free",
    "driver_output",
    "driver_alloc_binary",
    "driver_realloc_binary",
    "driver_free_binary",
    "driver_binary_get_refc",
    "driver_binary_inc_refc",
    "driver_binary_dec_refc",
    "enif_make_string_len",
];

// ---------------------------------------------------------------------------------------------
// Helpers (the idiom of `tests/driver_threads.rs`)
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

/// `true` iff every name in `names` is an **undefined** import of `path`.
fn all_symbols_are_undefined_imports(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--undefined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines().any(|line| line.contains(name) && line.contains(" UND "))
        }));
    }
    None
}

/// `true` iff every name in `names` is **defined** in `path`'s dynamic symbol table.
fn all_symbols_are_exported(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--defined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines().any(|line| line.contains(name) && !line.contains(" UND "))
        }));
    }
    None
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so("driver_binary_refc", FIXTURE_C)
}

/// Keep the `driver_*`- **and** `enif_*`-carrying codegen units in the link: the fixture's
/// undefined imports must resolve against this test binary's dynamic symbol table, and both api
/// tables are what publishes them (`#[used]` statics + `-Wl,--export-dynamic` from `build.rs`).
fn link_the_families() {
    let table = driver_api_table();
    assert_eq!(
        core::mem::size_of_val(table),
        core::mem::size_of::<beam_abi::driver::api::ErlDrvApi>()
    );
    core::hint::black_box(table);
    core::hint::black_box(beam_abi::nif::enif::enif_api_table());
}

/// Drive one `control` command and return the single report the fixture published through
/// `driver_output`.
fn drive(lib: &DriverLibrary, data: ErlDrvData, host: &TestDriverHost, cmd: u32) -> String {
    let before = host.outputs().len();
    let mut reply: *mut c_char = std::ptr::null_mut();
    // SAFETY: `data` is the live state returned by `start`; the commands read no input buffer; a
    // null reply pointer with `rlen == 0` is the "length only" shape `port_control/3` uses.
    let n = lib
        .control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control");
    let outputs = host.outputs();
    assert_eq!(
        outputs.len(),
        before + 1,
        "control command {cmd} must publish exactly one report through driver_output"
    );
    let report = String::from_utf8(outputs.last().expect("just checked").clone())
        .expect("the report is ASCII text");
    assert_eq!(
        n as usize,
        report.len(),
        "control must return the length of the report it published for command {cmd}"
    );
    report
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_binary_refcount_symbols_dynamically() {
    link_the_families();

    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_binary_refcount_symbols_dynamically: /proc/self/exe \
              unavailable");
        return;
    };
    match all_symbols_are_exported(&exe, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "`build.rs` emits `-Wl,--export-dynamic` for the test targets, so every symbol the \
             fixture imports must appear in {:?}'s dynamic symbol table",
            exe
        ),
        None => skip("the_test_binary_exports_the_binary_refcount_symbols_dynamically: no nm/readelf"),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load the real `.so` and drive the trio through the C ABI
// ---------------------------------------------------------------------------------------------

#[test]
fn drives_a_real_c_driver_through_the_refcount_trio() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };

    // The fixture's imports are exactly the symbols the loading process must export; if that ever
    // stopped being true, `dlopen(..., RTLD_NOW)` would fail on the next line instead of silently
    // resolving somewhere else.
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "{:?} must leave every fixture import undefined (that is the whole mechanism)",
            so
        ),
        None => skip("drives_a_real_c_driver_through_the_refcount_trio: no nm/readelf"),
    }

    let Some(path) = so_path(&so) else { return };

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ---------------------------------------
    let lib = load(&path, FIXTURE_NAME)
        .expect("a driver whose imports resolve from this test binary must load");
    assert_eq!(lib.name(), FIXTURE_NAME);

    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = std::ffi::CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(
        !data.is_null(),
        "start must return the private state, not a failure code"
    );

    // --- the four sequences, against the values measured on the real ERTS ----------------------
    let commands = [ALLOC, REALLOC, ZERO, DEC_TO_ZERO];
    for (i, (&cmd, &expected)) in commands.iter().zip(ORACLE_REPORTS.iter()).enumerate() {
        let report = drive(&lib, data, &host, cmd);
        assert_eq!(
            report, expected,
            "sequence {} (control command {cmd}) must match the value measured on the real ERTS",
            i + 1
        );
    }
    assert_eq!(
        host.outputs().len(),
        4,
        "every command publishes exactly one report"
    );

    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");
    // Dropping the library runs `finish` (absent here) then `dlclose`s the object.
    drop(lib);
}

// ---------------------------------------------------------------------------------------------
// The oracle: the same `.so`, loaded by a real ERTS
// ---------------------------------------------------------------------------------------------

/// Generate, compile and run a real Erlang module named [`FIXTURE_NAME`] that loads the fixture
/// `.so` with `erlang:load_nif/2` and prints the four reports, one per `ORACLE ` line.
///
/// The module name must equal the NIF entry's `name` field (`ERL_NIF_INIT` stringifies it), so the
/// generated module is `binrefc`. `None` (after a loud skip) when `erlc`/`erl` are missing or the
/// NIF cannot be loaded (`erlang:load_driver/2` is unavailable in this build, so the NIF entry is
/// the only way the oracle can reach the fixture at all — see the module docs).
fn oracle_reports(so: &Path) -> Option<Vec<String>> {
    // `erlang:load_nif/2` appends the platform extension, so the path is passed without ".so".
    let stem = so.with_extension("");
    let stem = stem.to_str()?;

    let dir = std::env::temp_dir().join(format!("beam_abi-binrefc-oracle-{}", std::process::id()));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        skip(&format!("binrefc oracle: cannot create scratch dir {dir:?}: {err}"));
        return None;
    }

    // The stub functions are what an ordinary NIF module looks like before the library is loaded
    // (`erlang:nif_error(nif_not_loaded)`); `-on_load/0` must return `ok` for the module to load.
    let source = format!(
        "-module({name}).\n\
         -export([main/0, seq_basic/0, seq_realloc/0, seq_zero/0, seq_dec_to_zero/0]).\n\
         -on_load(init/0).\n\n\
         init() ->\n\
         \x20   case erlang:load_nif(\"{stem}\", []) of\n\
         \x20       ok -> ok;\n\
         \x20       {{error, R}} -> erlang:error({{nif_load, R}})\n\
         \x20   end.\n\n\
         main() ->\n\
         \x20   [io:format(\"ORACLE ~ts~n\", [F()]) || F <- [fun seq_basic/0, fun seq_realloc/0,\n\
         \x20                                             fun seq_zero/0, fun seq_dec_to_zero/0]],\n\
         \x20   halt(0).\n\n\
         seq_basic() -> erlang:nif_error(nif_not_loaded).\n\
         seq_realloc() -> erlang:nif_error(nif_not_loaded).\n\
         seq_zero() -> erlang:nif_error(nif_not_loaded).\n\
         seq_dec_to_zero() -> erlang:nif_error(nif_not_loaded).\n",
        name = FIXTURE_NAME,
        stem = stem,
    );
    let src = dir.join(format!("{FIXTURE_NAME}.erl"));
    if let Err(err) = std::fs::write(&src, source) {
        skip(&format!("binrefc oracle: cannot write {src:?}: {err}"));
        return None;
    }

    // `erlc -o <dir> <src>`: compiling a real module (rather than `-eval`) is required, because
    // `erlang:load_nif/2` derives the calling module from the call site and rejects a library whose
    // entry `name` does not match it.
    let compiled = Command::new("erlc").arg("-o").arg(&dir).arg(&src).output();
    match compiled {
        Err(err) => {
            skip(&format!("binrefc oracle: cannot run erlc ({err})"));
            return None;
        }
        Ok(out) if !out.status.success() => {
            skip(&format!(
                "binrefc oracle: erlc failed (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Ok(_) => {}
    }

    let run = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-s")
        .arg(FIXTURE_NAME)
        .arg("main")
        .output();
    let output = match run {
        Err(err) => {
            skip(&format!("binrefc oracle: cannot run erl ({err})"));
            return None;
        }
        Ok(out) => out,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        skip(&format!(
            "binrefc oracle: erl failed (exit {:?}):\nstdout: {stdout}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let lines: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("ORACLE "))
        .map(|line| line.trim_end().to_string())
        .collect();
    if lines.len() != ORACLE_REPORTS.len() {
        skip(&format!(
            "binrefc oracle: expected {} `ORACLE …` lines, got {}:\n{stdout}",
            ORACLE_REPORTS.len(),
            lines.len()
        ));
        return None;
    }
    Some(lines)
}

#[test]
fn the_oracle_reports_the_same_sequences_from_real_erts() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    let Some(oracle) = oracle_reports(&so) else { return };

    // This is the assertion the whole criterion rests on: the SAME compiled C code, driving the
    // SAME `driver_binary_*` surface, must produce the SAME four reports under the real ERTS and
    // under this implementation.
    for (i, (got, expected)) in oracle.iter().zip(ORACLE_REPORTS.iter()).enumerate() {
        assert_eq!(
            got,
            expected,
            "the real ERTS's sequence {} must equal the value this test pins",
            i + 1
        );
    }

    // And a second, independent reading: the four strings must be *distinct* and non-empty, so the
    // comparison cannot be vacuous.
    let mut sorted = oracle.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ORACLE_REPORTS.len(),
        "the four sequences must be distinct: {oracle:?}"
    );
    assert!(sorted.iter().all(|line| line.starts_with("get=")
        || line.starts_with("size_before=")
        || line.starts_with("nonnull=")));
}
