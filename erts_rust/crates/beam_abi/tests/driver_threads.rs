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

//! End-to-end proof of the **threads & synchronisation** family of the driver C ABI (workspace
//! contract **A.4**): the **real C driver** `tests/fixtures/driver_threads.c` — compiled with the
//! system `cc` against the system `erl_driver.h` — is `dlopen`ed, validated, and driven through the
//! Rust loader, its `erl_drv_*` / `driver_*` imports resolving out of the *test binary's* dynamic
//! symbol table.
//!
//! # Why this is the load-bearing test
//!
//! The fixture calls the whole `erl_drv_mutex_*` / `erl_drv_cond_*` / `erl_drv_rwlock_*` /
//! `erl_drv_tsd_*` / `erl_drv_thread_*` surface, whose definitions live in *this* crate
//! (`beam_abi::driver::drv::threads`). The linker leaves those calls **undefined** in the `.so`
//! (verified below by inspecting its dynamic symbol table), so `dlopen(..., RTLD_NOW)` in
//! [`beam_abi::driver::loader::load`] can only succeed if the *loading process* exports them as
//! dynamic symbols — which `build.rs` arranges with `-Wl,--export-dynamic` on the test targets and
//! the `#[used] DRIVER_API` static keeps in the link. Every value the fixture *returns* was
//! therefore produced by the Rust implementation, so the assertions below pin the Rust behaviour
//! through the C ABI rather than through a Rust-side restatement of it.
//!
//! # Honest scope of the proof
//!
//! What is proven: a driver `.so` built against the unmodified header loads and its `control`
//! callback drives the Rust entry points through real C code — mutex create/name/trylock/lock/
//! unlock/destroy, a genuine cross-thread `erl_drv_cond_wait`/`signal`/`broadcast` handshake, the
//! read/write-lock round trip, per-thread TSD values across threads, thread create/name/join with
//! `erl_drv_thread_self`/`erl_drv_equal_tids` evaluated *inside* the worker, `erl_drv_thread_exit`
//! delivering its response, the thread-options record, and the four `*_name` accessors in all three
//! states: a handle created with a NULL name reports the C's static `"unknown"`, a named handle its
//! name, and a NULL handle `NULL` (commands 10-12).
//! What is *not* proven here: a real emulator port table behind the driver — the test host is
//! `common::TestDriverHost`, so "the port is really live in an emulator" is a `beam_rt` concern.
//!
//! # Oracle cross-check (and why it skips in this sandbox)
//!
//! [`the_oracle_runs_the_same_c_fixture_inside_real_erts`] drives the **same** `.so` through a real
//! `erl` (`erlang:load_driver/2` + `open_port({spawn_driver, …})` + `port_control/3`) and compares
//! the per-command reply lengths with the Rust-driven values. `erlang:load_driver/2` is a
//! *build-time* option of the emulator: the Nix-packaged OTP 29 / erts-17.1 used here does **not**
//! export it, so the test detects that and prints a loud `SKIP:` banner naming the reason. On a
//! driver-capable ERTS the comparison is hard (the same fixture's assertions must hold and the same
//! values must come back).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::{load, DriverLibrary};
use beam_abi::driver::types::ErlDrvData;
use beam_abi::DriverEnv;
use libc::{c_char, c_int};

use common::{compile_so, skip, TestDriverHost};

/// The committed C fixture — a real driver (see the file header for the A.4 rationale).
const FIXTURE_C: &str = include_str!("fixtures/driver_threads.c");

/// The driver name the fixture declares (`DRIVER_INIT(threads)` / `driver_name = "threads"`).
const FIXTURE_NAME: &str = "threads";

/// The `driver_*` / `erl_drv_*` symbols the fixture imports: every slot this family owns plus the
/// two host-free allocators the private state needs. They must be *undefined* in the `.so` and
/// *defined* in the loading process — that is the whole mechanism, so it is checked directly.
const FIXTURE_IMPORTS: [&str; 35] = [
    "driver_alloc",
    "driver_free",
    "erl_drv_mutex_create",
    "erl_drv_mutex_destroy",
    "erl_drv_mutex_lock",
    "erl_drv_mutex_trylock",
    "erl_drv_mutex_unlock",
    "erl_drv_mutex_name",
    "erl_drv_cond_create",
    "erl_drv_cond_destroy",
    "erl_drv_cond_signal",
    "erl_drv_cond_broadcast",
    "erl_drv_cond_wait",
    "erl_drv_cond_name",
    "erl_drv_rwlock_create",
    "erl_drv_rwlock_destroy",
    "erl_drv_rwlock_tryrlock",
    "erl_drv_rwlock_rlock",
    "erl_drv_rwlock_runlock",
    "erl_drv_rwlock_tryrwlock",
    "erl_drv_rwlock_rwlock",
    "erl_drv_rwlock_rwunlock",
    "erl_drv_rwlock_name",
    "erl_drv_tsd_key_create",
    "erl_drv_tsd_key_destroy",
    "erl_drv_tsd_set",
    "erl_drv_tsd_get",
    "erl_drv_thread_opts_create",
    "erl_drv_thread_opts_destroy",
    "erl_drv_thread_create",
    "erl_drv_thread_self",
    "erl_drv_equal_tids",
    "erl_drv_thread_exit",
    "erl_drv_thread_join",
    "erl_drv_thread_name",
];

/// Every `control` command the fixture answers with `1` when its assertions held, plus the
/// documented no-op (`0` → `0`) and the unknown-command marker (`99` → `-100`).
const COMMANDS: [(u32, c_int); 14] = [
    (0, 0),
    (1, 1),
    (2, 1),
    (3, 1),
    (4, 1),
    (5, 1),
    (6, 1),
    (7, 1),
    (8, 1),
    (9, 1),
    (10, 1),
    (11, 1),
    (12, 1),
    (99, -100),
];

/// The commands the *oracle* may drive: everything except command 8, whose refusals are this
/// layer's documented behaviour where the C `erts_exit`s (`erl_drv_thread.c:523,545,670,692,712`),
/// and except command 9, which needs the `start`-returned private state that a `spawn_driver` port
/// does provide but whose assertion is Rust-side only.
///
/// Commands 10-12 (the `*_name` accessors on an unnamed, a named and a NULL handle) **are** driven by
/// the oracle: they are exactly the C contract `erl_drv_thread.c:99,135,170,198,264,283,341,365,678`
/// defines, so both runtimes must answer the same on them.
const ORACLE_COMMANDS: [u32; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 99];

// ---------------------------------------------------------------------------------------------
// Helpers (the same idiom as `tests/driver_lifecycle.rs`)
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
    compile_so("driver_threads", FIXTURE_C)
}

/// Drive one `control` command that needs no buffers, expecting the fixture to answer.
fn command(lib: &DriverLibrary, data: ErlDrvData, cmd: u32) -> c_int {
    let mut reply: *mut c_char = std::ptr::null_mut();
    // SAFETY: `data` is the live state from `start`; the commands driven here read no buffer, and a
    // null reply pointer with `rlen == 0` is what `port_control/3` passes when it wants a length.
    lib.control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control") as c_int
}

/// Keep the `driver_*`-carrying codegen unit in the link (the `#[used]` static publishes the
/// `#[no_mangle]` symbols for `--export-dynamic`). Deliberately *not* an assertion on the slot
/// contents: the family's own `#[cfg(test)]` unit test is the single owner of that list.
fn link_the_family() {
    let table = driver_api_table();
    assert_eq!(
        core::mem::size_of_val(table),
        core::mem::size_of::<beam_abi::driver::api::ErlDrvApi>()
    );
    core::hint::black_box(table);
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_thread_symbols_dynamically() {
    link_the_family();

    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_thread_symbols_dynamically: /proc/self/exe unavailable");
        return;
    };
    match all_symbols_are_exported(&exe, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "`build.rs` emits `-Wl,--export-dynamic` for the test targets, so every symbol in the \
             family must appear in {:?}'s dynamic symbol table",
            exe
        ),
        None => skip("the_test_binary_exports_the_thread_symbols_dynamically: no nm/readelf"),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load the real driver `.so` and drive the family through the C ABI
// ---------------------------------------------------------------------------------------------

#[test]
fn loads_and_drives_a_real_c_driver_through_the_threads_family() {
    link_the_family();

    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // --- the `.so` really imports this family (bound at load time, not at link time) ------------
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "the fixture must reference the whole family as undefined dynamic symbols; inspect {} \
             with `nm -D`",
            so.display()
        ),
        None => skip("loads_and_drives_a_real_c_driver_through_the_threads_family: no nm/readelf"),
    }

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ----------------------------------------
    let lib = load(&path, FIXTURE_NAME)
        .expect("a real driver must load: its erl_drv_* imports resolve from the test binary");
    assert_eq!(lib.name(), FIXTURE_NAME);

    // --- the port and the private state ---------------------------------------------------------
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command_line = std::ffi::CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; the command is NUL-ended.
    let data = lib
        .start(port, command_line.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null(), "start must return the private state, not a failure code");

    // The threads family touches no `DriverHost` service, so `TestDriverHost`'s recording surface
    // must stay untouched by the whole run — the C ABI is self-contained here.
    for (cmd, expected) in COMMANDS {
        assert_eq!(
            command(&lib, data, cmd),
            expected,
            "command {cmd}: the fixture's assertions must hold under the Rust implementation"
        );
    }
    assert_eq!(host.outputs().len(), 0, "the threads family writes no port output");
    assert_eq!(host.sent().len(), 0, "the threads family sends no terms");
    assert_eq!(host.selects().len(), 0, "the threads family selects no events");
    assert_eq!(host.timers().len(), 0, "the threads family arms no timers");
    assert_eq!(host.asyncs(), 0, "the threads family uses no async jobs");

    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");
    drop(lib);
}

// ---------------------------------------------------------------------------------------------
// The oracle: the same `.so`, driven by a real ERTS through `erlang:load_driver/2`
// ---------------------------------------------------------------------------------------------

/// The fixture's `control` reply length for each oracle command, as an Erlang term list.
fn oracle_expected(commands: &[u32]) -> String {
    let pairs: Vec<String> = commands
        .iter()
        .map(|cmd| format!("{{{}, {}}}", cmd, COMMANDS.iter().find(|(c, _)| c == cmd).unwrap().1))
        .collect();
    format!("[{}]", pairs.join(","))
}

#[test]
fn the_oracle_runs_the_same_c_fixture_inside_real_erts() {
    link_the_family();

    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // One line, so it survives `-eval` (which is a single expression).
    let list = format!(
        "[{}]",
        ORACLE_COMMANDS.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
    );
    let eval = format!(
        "R = case erlang:function_exported(erlang, load_driver, 2) of \
           false -> oracle_no_load_driver; \
           true -> ok = erlang:load_driver(\"{path}\", []), \
                   P = open_port({{spawn_driver, \"{name}\"}}, []), \
                   [{{C, try length(port_control(P, C, <<>>)) catch _:E -> {{error, E}} end}} \
                    || C <- {list}] \
         end, io:format(\"ORACLE ~p~n\", [R]), halt(0).",
        path = path,
        name = FIXTURE_NAME,
        list = list,
    );

    let output = Command::new("erl").args(["-noshell", "-eval", &eval]).output();
    let Ok(output) = output else {
        skip("the_oracle_runs_the_same_c_fixture_inside_real_erts: no `erl` on PATH");
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("oracle_no_load_driver") {
        skip(
            "the_oracle_runs_the_same_c_fixture_inside_real_erts: this ERTS build does not export \
             erlang:load_driver/2 (a build-time emulator option), so the C fixture cannot be loaded \
             into it; the Rust-driven run above is the only available evidence here",
        );
        return;
    }
    if !output.status.success() {
        panic!(
            "the oracle `erl` run failed (exit {:?}):\nstdout: {stdout}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("ORACLE "))
        .unwrap_or_else(|| panic!("no `ORACLE …` line in the oracle output:\n{stdout}"));
    assert_eq!(
        line.trim(),
        oracle_expected(&ORACLE_COMMANDS),
        "the fixture's per-command reply lengths must be identical under real ERTS and under this \
         implementation"
    );
}
