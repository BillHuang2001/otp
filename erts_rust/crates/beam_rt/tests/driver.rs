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

//! The **driver** end-to-end test (contract A.4): a real C driver `.so` is `dlopen`ed and driven
//! from `beam_rt`'s own [`DriverHost`](beam_rt::port::driver::DriverPort) implementation.
//!
//! Everything here is real: the C source is the fixture `beam_abi`'s own end-to-end test compiles
//! (so the two layers are pinned against *one* driver), it is compiled with the host C compiler
//! against the host `erl_driver.h`, and the resulting shared object is loaded through
//! [`beam_abi::driver::loader`] exactly as `erlang:load_driver/2` would.
//!
//! # Why this is the load-bearing test for `beam_rt`
//!
//! `beam_abi` proves the driver ABI *shape* and its `driver_*` host functions in isolation. What is
//! new here is the other half of the seam: those host functions run against **this** runtime's
//! [`DriverHost`](beam_rt::port::driver::DriverPort) — a driver-private term arena, the runtime's
//! shared atom table, and an owning pid — and the bytes a driver hands to `driver_output` are
//! captured by the runtime. The fixture's `output` callback echoes whatever Erlang sent, so a
//! captured payload is direct evidence that the C callback reached the Rust host.
//!
//! # The `--export-dynamic` link seam
//!
//! The fixture leaves `driver_alloc`/`driver_output`/… **undefined**; `dlopen(RTLD_NOW)` resolves
//! them out of the loading process's dynamic symbol table. `build.rs` emits
//! `-Wl,--export-dynamic` for the binary and test targets, and
//! [`PortRegistry::new`](beam_rt::port::PortRegistry::new) anchors `beam_abi`'s driver codegen unit
//! with a `black_box` reference to `beam_abi::driver::drv::driver_api_table()` — without either
//! half, the `dlopen` fails on an unresolved symbol. Two tests below pin both halves: the byte
//! capture proves the load succeeded, and `nm -D`/`readelf --dyn-syms` proves the symbols are
//! exported.
//!
//! # Skip policy
//!
//! Three host facilities are needed (the C compiler, the host `erl_driver.h`, and `nm`/`readelf`
//! for the symbol proof). When one is missing the affected test prints a **loud** banner and
//! returns — it never silently passes.
use std::ffi::CString;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use beam_abi::driver::types::ERL_DRV_EXTENDED_MAJOR_VERSION;
use beam_alloc::Heap;
use beam_engine::terms;
use beam_process::ProcessId;
use beam_rt::port::driver::{load_driver, Driver, DriverPort};
use beam_rt::Runtime;
use beam_term::prelude::{is_atom, is_small, is_tuple, signed_value, tuple_element, tuple_size};
use beam_term::AtomTable;

/// The `beam_abi` driver fixture, reused verbatim so both layers are pinned against one `.so`.
///
/// Its `ErlDrvEntry::driver_name` is `"t"`, so the only name it may be loaded as is `t`.
const DRIVER_C: &str = include_str!("../../beam_abi/tests/fixtures/test_driver.c");

/// The `driver_*` / `erl_drv_*` symbols the fixture imports — the `beam_rt` binary must export
/// every one of them as a **dynamic** symbol for `dlopen(RTLD_NOW)` to bind the library.
const DRIVER_IMPORTS: &[&str] = &[
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

/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// ---------------------------------------------------------------------------------------------
// Locating the host `erl_driver.h`
// ---------------------------------------------------------------------------------------------

/// The header the include-directory search keys on.
const HEADER: &str = "erl_driver.h";

/// Locate the host `erl_driver.h` include directory, robustly; `None` if unavailable.
///
/// Resolution order (the first directory that actually contains a readable `erl_driver.h` wins):
/// ask the host `erl`; then canonicalize the `erl` on `PATH`; then probe the standard roots.
fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval()
        .or_else(from_path_erl)
        .or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}

/// True when `dir` holds a readable `erl_driver.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// Ask the host `erl` for `code:lib_dir(erts, include)`.
fn from_erl_eval() -> Option<PathBuf> {
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir(erts,include)])",
            "-s",
            "init",
            "stop",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let dir = PathBuf::from(text.trim());
    has_header(&dir).then_some(dir)
}

/// Canonicalize the `erl` on `PATH` and take the sibling `include` of its `erts-*` directory.
fn from_path_erl() -> Option<PathBuf> {
    let erl = which_erl()?.canonicalize().ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// The first `erl` executable on `PATH`.
fn which_erl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("erl"))
        .find(|candidate| candidate.is_file())
}

/// The standard installation roots to probe.
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top));
    }
    for base in [
        "/usr/lib/erlang",
        "/usr/local/lib/erlang",
        "/usr/lib64/erlang",
        "/opt/erlang",
    ] {
        roots.push(PathBuf::from(base));
    }
    roots
}

/// Search one installation root for an include directory holding `erl_driver.h`.
fn search_root(root: &Path) -> Option<PathBuf> {
    let direct = root.join("include");
    if has_header(&direct) {
        return Some(direct);
    }
    let lib_erlang = root.join("lib").join("erlang");
    for base in [root.to_path_buf(), lib_erlang.clone()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if has_header(&include) {
                return Some(include);
            }
        }
    }
    let include = lib_erlang.join("include");
    has_header(&include).then_some(include)
}

// ---------------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------------

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Whether a C compiler is available.
fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// A compiled driver `.so` in a scratch directory.
struct Fixture {
    /// The scratch directory holding the source and the compiled `.so`.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the `load_driver` candidate; the loader appends `.so`).
    stem: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The candidate paths [`load_driver`] should try, in order: the stem and its `.so` spelling.
    fn candidates(&self) -> Vec<String> {
        vec![
            self.stem.display().to_string(),
            format!("{}.so", self.stem.display()),
        ]
    }
}

/// Compiles the committed C fixture, or `None` when a host facility is missing (the caller returns
/// early — **loudly**).
fn build_fixture(label: &str) -> Option<Fixture> {
    compile_c(label, "t", DRIVER_C)
}

/// Compiles `source` into `<scratch>/<stem_name>.so`, or `None` when the toolchain is missing.
fn compile_c(label: &str, stem_name: &str, source: &str) -> Option<Fixture> {
    let Some(include) = erl_include_dir() else {
        skip(&format!("driver/{label}: no erl_driver.h include directory found"));
        return None;
    };
    if !cc_available() {
        skip(&format!(
            "driver/{label}: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_driver_{label}_{}_{n}", std::process::id()));
    let stem = dir.join(stem_name);
    let c_src = dir.join(format!("{stem_name}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, source));
    if let Err(error) = write {
        skip(&format!("driver/{label}: cannot prepare {}: {error}", dir.display()));
        return None;
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`.
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem_name}.so")))
        .arg(&c_src)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "driver/{label}: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("driver/{label}: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !dir.join(format!("{stem_name}.so")).is_file() {
        skip(&format!(
            "driver/{label}: the C compiler produced no {stem_name}.so"
        ));
        return None;
    }
    Some(Fixture { dir, stem })
}

/// Compiles `source` as `<dir>/<stem_name>.erl` with the system `erlc`, or `None` when the
/// compiler is missing (**loudly**) — the same skip policy as [`compile_c`].
///
/// The `.erl` is the *Erlang* half of the driver end-to-end test: it interpolates the compiled
/// driver's path (only known at test time) so the fixture is never committed.
fn compile_erl(dir: &Path, stem_name: &str, source: &str) -> Option<PathBuf> {
    let source_path = dir.join(format!("{stem_name}.erl"));
    if let Err(error) = std::fs::write(&source_path, source) {
        skip(&format!("driver/erl: cannot write {}: {error}", source_path.display()));
        return None;
    }
    let output = match Command::new("erlc").arg("-o").arg(dir).arg(&source_path).output() {
        Ok(output) => output,
        Err(error) => {
            skip(&format!("driver/erl: cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip(&format!(
            "driver/erl: `erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join(format!("{stem_name}.beam"));
    if !beam.is_file() {
        skip(&format!("driver/erl: `erlc` produced no {}", beam.display()));
        return None;
    }
    Some(beam)
}

/// The Erlang half of the end-to-end test: open a real C driver port, `port_command` it, and
/// return what its **owner** receives together with two `port_info/2` answers.
///
/// `{path}` is replaced by the compiled driver's path (without the `.so` suffix) at test time.
/// The `{spawn_driver, Path}` form is this runtime's Milestone-1 extension
/// ([`beam_rt::port::driver_lookup`]): the C requires the object to be loaded first with a
/// `load_driver` BIF this runtime does not have, so the path doubles as the search.
const DRV_RECV_ERL: &str = r#"%% Generated by `beam_rt/tests/driver.rs` (never committed).
-module(drv_recv).
-export([recv/0]).

recv() ->
    Port = open_port({spawn_driver, "{path}"}, [binary]),
    true = port_command(Port, <<"hello">>),
    Data = receive {Port, {data, D}} -> D end,
    {Data, erlang:port_info(Port, os_pid), erlang:port_info(Port, name)}.
"#;// ---------------------------------------------------------------------------------------------
// Symbol-table inspection
// ---------------------------------------------------------------------------------------------

/// Run `cmd args…`, returning its stdout on success (`None` when the tool is missing or fails).
fn run_tool(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Whether every name in `names` is **defined** in `path`'s dynamic symbol table (i.e. exported),
/// or `None` when neither `nm` nor `readelf` is usable.
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

/// Whether every name in `names` is an **undefined** import of `path`, or `None` when neither `nm`
/// nor `readelf` is usable.
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

/// The dynamic symbol table of `binary`, or `None` (loudly) when no tool can read it.
fn dynamic_symbols(binary: &Path) -> Option<String> {
    let nm = Command::new("nm").arg("-D").arg(binary).output();
    if let Ok(output) = nm {
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    let readelf = Command::new("readelf")
        .arg("--dyn-syms")
        .arg("-W")
        .arg(binary)
        .output();
    if let Ok(output) = readelf {
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    skip("driver/symbols: neither `nm -D` nor `readelf --dyn-syms` could read the binary");
    None
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// A host over a table that already holds the fixture's one atom (`t`), running as pid 1.
fn fixture_host(atoms: &Rc<AtomTable>) -> DriverPort {
    DriverPort::new(
        Heap::new(4096).expect("a driver heap is allocatable"),
        Rc::clone(atoms),
        ProcessId::new(1, 0),
    )
}

/// Read the atom/tuple `{t, 1}` the fixture's `control(5)` built through `erl_drv_output_term`.
fn assert_is_the_t_tuple(host: &DriverPort, term: beam_term::prelude::Eterm) {
    // SAFETY: `term` was built on `host`'s heap by `decode_term_spec` and the arena has not been
    // reset (and is never reset), so it is a live 2-tuple.
    assert!(unsafe { is_tuple(term) }, "erl_drv_output_term must have built a tuple");
    // SAFETY: as above — a live tuple.
    assert_eq!(unsafe { tuple_size(term) }, 2, "{term:?} is not a 2-tuple");
    // SAFETY: element 0 of the live 2-tuple.
    let key = unsafe { tuple_element(term, 0) };
    // SAFETY: element 1 of the live 2-tuple.
    let value = unsafe { tuple_element(term, 1) };
    assert!(is_atom(key), "the tuple key must be the atom `t`");
    // `t` was seeded into the shared table, so `intern_atom` returned the seeded index.
    assert_eq!(host.atom_name(key).as_deref(), Some("t"));
    assert!(is_small(value), "the tuple value must be the small int 1");
    assert_eq!(signed_value(value), 1);
}

/// A real C driver `.so` loads and is driven: `start` allocates private state through
/// `driver_alloc`, `output` echoes through `driver_output`, `control` runs the driver's own
/// commands, and every byte lands in `beam_rt`'s [`DriverPort`].
#[test]
fn loads_and_drives_a_real_c_driver() {
    // Load-bearing: this pins the frozen `driver_*` table AND keeps the `driver_*`-carrying codegen
    // unit in the test link (without it the `.so`'s imports would not resolve and `dlopen` would
    // fail). `PortRegistry::new` does the same for the `beam_rt` binary.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());

    let Some(fixture) = build_fixture("drive") else {
        return;
    };

    // --- the `.so` really imports `driver_*` (bound at load time, not at link time) -------------
    match all_symbols_are_undefined_imports(&fixture.stem.with_extension("so"), DRIVER_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "the fixture must reference {} as undefined dynamic symbols; inspect its `.so` with \
             `nm -D`",
            DRIVER_IMPORTS.join("/")
        ),
        None => skip("driver/drive: no nm/readelf to inspect the `.so`'s dynamic symbols"),
    }    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ----------------------------------------
    // The name is the entry's own `driver_name`; a mismatch would be `bad_driver_name`.
    let library = load_driver(&fixture.candidates(), "t")
        .expect("a real driver must load: its driver_* imports resolve from the test binary");
    assert_eq!(library.name(), "t");
    // The fixture declares no `init` callback, so the load path ran none (`load` invokes it iff
    // present — the C's `if (!de->init) return 0`).
    assert!(library.init().is_none(), "the fixture declares no init callback");

    let mut table = AtomTable::new();
    table.intern("t");
    let atoms = Rc::new(table);
    let mut driver = Driver::new(library, fixture_host(&atoms));

    // --- the one per-port environment + `start` -------------------------------------------------
    // `env` must outlive every use of `port`; `driver` is not moved, so its boxed host is stable.
    let mut env = driver.host_mut().environment();
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");

    // SAFETY: `port` came from `env.as_port()` on a live environment; `command` is NUL-terminated.
    let data = driver
        .library()
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null(), "start must return the private state, not a failure code");

    // --- `output` echoes through `driver_output` into this runtime's host ------------------------
    let hello = b"hello\0world";
    // SAFETY: `data` is the live state from `start`; `hello` is a readable buffer of `len` bytes.
    driver
        .library()
        .output(data, hello.as_ptr() as *mut c_char, hello.len())
        .expect("the fixture implements output");
    assert_eq!(
        driver.host().outputs(),
        &[(Vec::new(), Some(hello.to_vec()))],
        "driver_output must deliver the received bytes to this runtime's host"
    );

    // --- `control(5)`: the term-data path builds {t, 1} on this runtime's heap -------------------
    let mut reply: *mut c_char = std::ptr::null_mut();
    // SAFETY: `data` is live; command 5 reads no buffer.
    let n = driver
        .library()
        .control(data, 5, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control");
    assert_eq!(n, 0, "erl_drv_output_term must succeed");
    let sent = driver.host().sent();
    assert_eq!(sent.len(), 1, "erl_drv_output_term must deliver exactly one term");
    assert!(
        sent[0].0.is_none(),
        "erl_drv_output_term targets the port owner, not a specific process"
    );
    assert_is_the_t_tuple(driver.host(), sent[0].1);

    // --- `control(6)`: a host-free `driver_*` call reports the frozen ABI version ----------------
    // SAFETY: `data` is live; command 6 reads no buffer.
    let major = driver
        .library()
        .control(data, 6, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("control");
    assert_eq!(major, ERL_DRV_EXTENDED_MAJOR_VERSION as isize);

    // --- `control(2)`: an *unsupported* host service reports the C's own failure code ------------
    // `driver_set_timer` is left at the `DriverHost` default (`-1`): the gap is typed, never a
    // fabricated success.
    // SAFETY: `data` is live; command 2 reads no buffer.
    let timer = driver
        .library()
        .control(data, 2, std::ptr::null_mut(), 0, &mut reply, 0)
        .expect("control");
    assert_eq!(timer, -1, "an unimplemented timer service must report -1, not 0");

    // --- `stop` frees the private state ---------------------------------------------------------
    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    driver.library().stop(data).expect("the fixture implements stop");

    drop(env);
    fixture.cleanup();
}

/// A driver that reports an unsupported host service never sees a fabricated success: the fixture's
/// `control(2)`/`control(3)`/`control(4)`/`control(7)` all funnel through the host's select/timer/
/// async slots, and every one of them is the C's failure code.
#[test]
fn the_unimplemented_host_services_report_the_c_failure_codes() {
    let Some(fixture) = build_fixture("gaps") else {
        return;
    };
    let library = load_driver(&fixture.candidates(), "t").expect("a real driver must load");
    let mut table = AtomTable::new();
    table.intern("t");
    let atoms = Rc::new(table);
    let mut driver = Driver::new(library, fixture_host(&atoms));

    let mut env = driver.host_mut().environment();
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");
    // SAFETY: `port` is from a live environment; `command` is NUL-terminated.
    let data = driver
        .library()
        .start(port, command.as_ptr() as *mut c_char)
        .expect("start");
    assert!(!data.is_null());

    let mut reply: *mut c_char = std::ptr::null_mut();
    for (command_id, expected) in [(2, -1), (3, -1), (4, -1), (7, -1)] {
        // SAFETY: `data` is live and none of these commands reads a buffer.
        let got = driver
            .library()
            .control(data, command_id, std::ptr::null_mut(), 0, &mut reply, 0)
            .expect("control");
        assert_eq!(
            got, expected,
            "control({command_id}) must report the host's documented gap"
        );
    }

    // SAFETY: `data` is the live state from `start`.
    driver.library().stop(data).expect("stop");
    drop(env);
    fixture.cleanup();
}

/// The load path really invokes the entry's `init` callback.
///
/// `beam_abi::driver::loader::load` runs the entry's `init` exactly where the C's `init_driver`
/// does (`io.c`: at load, after the version/name validation), and a non-zero result vetoes the
/// load with `driver_init_failed`. A driver whose `init` always vetoes therefore proves the
/// callback ran — the load cannot succeed without calling it.
#[test]
fn the_load_path_runs_the_entries_init_callback() {
    const VETOING_INIT_C: &str = r#"
#include <erl_driver.h>

static int t_init(void) { return 1; }

static ErlDrvEntry t_driver_entry = {
    .init = t_init,
    .driver_name = "t",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(t) { return &t_driver_entry; }
"#;
    let Some(fixture) = compile_c("init", "t", VETOING_INIT_C) else {
        return;
    };
    let error = load_driver(&fixture.candidates(), "t")
        .expect_err("an `init` that returns non-zero must veto the load");
    assert_eq!(
        error.reason_atom(),
        "driver_init_failed",
        "the veto must be reported as the oracle's driver_init_failed: {error:?}"
    );
    fixture.cleanup();
}

/// The built binary **exports** every `driver_*` symbol the fixture imports, as a dynamic symbol.///
/// This is the other half of the link-time seam (contract A.4): `build.rs` passes
/// `-Wl,--export-dynamic` for the binary and test targets, and `PortRegistry::new` anchors
/// `beam_abi`'s driver codegen unit — without either, `dlopen(RTLD_NOW)` of a real driver would
/// fail on an unresolved symbol. Skips loudly when neither `nm` nor `readelf` is installed.
#[test]
fn the_binary_exports_the_driver_symbols_a_driver_imports() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_beam_rt"));
    let symbols = match dynamic_symbols(&binary) {
        Some(text) => text,
        None => return,
    };
    for symbol in DRIVER_IMPORTS {
        assert!(
            symbols.contains(symbol),
            "the dynamic symbol table of {} has no `{symbol}`",
            binary.display()
        );
    }
}

/// The same export proof taken directly from the binary's dynamic symbol table, cross-checked with
/// the "defined, not undefined" filter so an import could never masquerade as an export.
#[test]
fn the_driver_symbols_are_defined_in_the_binary_not_imported() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_beam_rt"));
    match all_symbols_are_exported(&binary, DRIVER_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "`build.rs` emits `-Wl,--export-dynamic` for the binary, so {} must be defined in \
             {}'s dynamic symbol table",
            DRIVER_IMPORTS.join("/"),
            binary.display()
        ),
        None => skip("driver/symbols: no nm/readelf available"),
    }
}

// ---------------------------------------------------------------------------------------------
// The end-to-end delivery path: a `{spawn_driver, Name}` port → its owner's mailbox
// ---------------------------------------------------------------------------------------------

/// A real C driver port opened by **Erlang** delivers the driver's `driver_output` bytes to its
/// **owning process** as `{Port, {data, Data}}`.
///
/// This is the load-bearing end-to-end proof of the A.4 port lifecycle: `tests/port_wake.rs`
/// proves a *spawn* port's pipe bytes wake a blocked receiver, and
/// [`loads_and_drives_a_real_c_driver`] proves the driver ABI host captures `driver_output`; this
/// test joins them. A `{spawn_driver, Path}` port is opened from interpreted code
/// ([`Runtime::spawn_process`]), `port_command` reaches the driver's `output` callback (which
/// **echoes** through `driver_output`), the captured bytes are queued as the port's input, and
/// [`Runtime::run_scheduler`] delivers them as `{Port, {data, …}}` into the owner's mailbox —
/// which is the *only* thing that can wake the receiver blocked on that pattern.
///
/// The same test also pins the two `port_info/2` answers that differ for a driver port: it has
/// **no OS process** (`{os_pid, undefined}` — the oracle's own answer for a driver/fd port), while
/// its `name` is the `Name` string the port was opened with (the driver's **path** in this
/// runtime's Milestone-1 `{spawn_driver, Path}` form; see [`beam_rt::port::driver_lookup`]).
///
/// Oracle ground truth (OTP 29 / `erts-17.1`), with the fixture loaded through
/// `erl_ddll:load_driver/2` and opened as `{spawn_driver, "t"}`:
///
/// ```none
/// $ erl -noshell -eval '… P = open_port({spawn_driver,"t"},[binary]),
///       true = port_command(P, <<"hello">>),
///       io:format("~p~n",[receive {P,{data,D}} -> D end]),
///       io:format("~p~n",[erlang:port_info(P,os_pid)]), halt(0).'
/// <<"hello">>
/// {os_pid,undefined}
/// ```
#[test]
fn a_spawn_driver_port_delivers_driver_output_to_its_owner() {
    // The driver ABI link seam (see `loads_and_drives_a_real_c_driver`): this test links the
    // `driver_*` codegen unit for the `.so`'s imports too.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());

    let Some(fixture) = build_fixture("e2e") else {
        return;
    };
    // The Erlang fixture needs the driver's path (the `.so` needs no suffix in the name: the
    // loader appends it), which only exists now.
    let source = DRV_RECV_ERL.replace("{path}", &fixture.stem.display().to_string());
    let Some(beam) = compile_erl(&fixture.dir, "drv_recv", &source) else {
        fixture.cleanup();
        return;
    };

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(&beam).expect("the driver fixture loads");
    let pid = rt
        .spawn_process("drv_recv", "recv", vec![])
        .expect("spawn drv_recv:recv/0");

    // Drive the scheduler until the receiver returns. The receiver opens the port and issues
    // `port_command` inline (whose `output` callback captures the driver's echo), then blocks in
    // `receive`; the same scheduler pass's port half delivers `{Port, {data, Data}}` into its
    // mailbox and resumes it. The bounded loop keeps the test independent of any startup latency
    // rather than asserting a single-call fixpoint.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "the driver port never delivered data to wake its owner"
        );
        rt.run_scheduler().expect("the scheduler pass runs");
        sleep(Duration::from_millis(5));
    }

    // The receiver returned `{Echo, {os_pid, OsPid}, {name, Name}}`.
    let result = rt.process_result(pid).expect("the receiver returned");
    let echo = terms::tuple_element(result, 0).expect("element 0 is the echo");
    assert_eq!(
        terms::binary_bytes(echo).expect("the driver echo is a binary"),
        b"hello",
        "the driver's `driver_output` bytes must reach the owner as `{{Port, {{data, Data}}}}`"
    );

    // `port_info(Port, os_pid)` is `undefined` for a driver port (no OS child) — the oracle's own
    // answer. Rendered through the runtime's own `~w` printer so no atom index is assumed.
    let os_pid = terms::tuple_element(result, 1).expect("element 1 is the os_pid answer");
    let os_pid_value = terms::tuple_element(os_pid, 1).expect("{os_pid, Value}");
    assert_eq!(
        rt.format_term_w(os_pid_value, None).expect("printable"),
        "undefined\n",
        "a driver port has no OS process"
    );

    // `port_info(Port, name)` is the `Name` string the port was opened with — here the driver's
    // path (this runtime's `{spawn_driver, Path}` form).
    let name = terms::tuple_element(result, 2).expect("element 2 is the name answer");
    let name_value = terms::tuple_element(name, 1).expect("{name, Value}");
    let text: String = terms::list_elements(name_value)
        .expect("the port name is a char list")
        .into_iter()
        .map(|c| {
            char::from(u8::try_from(signed_value(c)).expect("a char-list byte is a byte-sized int"))
        })
        .collect();
    assert_eq!(text, fixture.stem.display().to_string(), "port_info(Port, name)");

    // A returning process exits `normal` (its `x(0)` is not its exit reason) and leaves the table.
    assert!(rt.process_exit(pid).is_none());
    assert!(rt.processes().get(pid).is_none());

    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// The driver event/timer surface: `driver_select` → `ready_input`, `driver_set_timer` → `timeout`
// ---------------------------------------------------------------------------------------------

/// The committed C source of the **event/timer** fixture, compiled at test time by
/// [`compile_c`] under the stem `event` (the `driver_name` its entry declares).
const EVENT_DRIVER_C: &str = include_str!("fixtures/event_driver.c");

/// The committed Erlang half of the same test; `{path}` is replaced by the compiled driver's path
/// (see `tests/fixtures/README.md` — the path only exists at test time, so no `.beam` is committed).
const EVENT_DRIVER_ERL: &str = include_str!("fixtures/event_driver.erl");

/// The two driver callbacks the runtime's **poller** has to drive for a driver port reach the port's
/// owner: the descriptor the driver selected with `driver_select` runs its `ready_input` when it
/// becomes readable (`"R"`), and the deadline it armed with `driver_set_timer` runs its `timeout` at
/// its deadline (`"T"`).
///
/// # Why this is the only proof of the event/timer surface
///
/// The two services are the only `DriverHost` requests that cannot be answered by a value the host
/// captures: both must reach the runtime's poller — `driver_select` registers the driver's own
/// descriptor with it, and `driver_set_timer`'s deadline has to be mirrored into the port's
/// `TIMEOUT` task and clamp the poll round. [`beam_abi`] cannot test that (it has no poller), and
/// the in-module `beam_rt` tests use a Rust recorder driver, not a real C driver. Here a real
/// driver object is `dlopen`ed, its `start` callback really arms a select on one end of an internal
/// `pipe(2)` and a 50 ms timer, and Erlang drives it through `Runtime`:
///
/// 1. `port_command(<<"go">>)` reaches the driver's `output`, which writes one byte to the pipe's
///    write end — making the **selected** read end readable, which is the only reason the runtime's
///    poller reports it. The scheduled `INPUT` task then runs the driver's `ready_input` (draining
///    the byte: the poller is edge-triggered), which answers `driver_output("R")`.
/// 2. The port timer fires at its deadline and runs the driver's `timeout`, which answers
///    `driver_output("T")`.
///
/// Both bytes reach the owner as `{Port, {data, Data}}` messages, and the fixture collects the two
/// in arrival order (`tests/fixtures/event_driver.erl`), so the returned `{Read, Timer}` tuple is
/// direct evidence of both callbacks having run. Either one missing shows up as the atom `timeout`
/// in that position (each `receive` is bounded), so a failure names the path that did not fire.
///
/// Oracle ground truth (OTP 29 / `erts-17.1`, verified with this very fixture `.so`): the C
/// `driver_select` registers the descriptor with the emulator's poll set (`erl_check_io.c`) and
/// `driver_set_timer` with its timer wheel (`erl_hl_timer.c`), so `ready_input`/`timeout` run
/// exactly there too:
///
/// ```none
/// $ erl -noshell -eval 'ok = erl_ddll:load_driver("<dir>", "event"),
///       P = open_port({spawn_driver, "event"}, [binary]),
///       true = port_command(P, <<"go">>),
///       A = receive {P,{data,D1}} -> D1 after 5000 -> timeout end,
///       B = receive {P,{data,D2}} -> D2 after 5000 -> timeout end,
///       io:format("~p ~p~n", [A, B]), halt(0).'
/// <<"R">> <<"T">>
/// ```
#[test]
fn a_driver_select_and_a_driver_timer_both_reach_the_owner() {
    // The A.4 link seam: the `.so` leaves its `driver_*` imports undefined, so the test binary must
    // publish them (see `the_binary_exports_the_driver_symbols_a_driver_imports`).
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());

    let Some(fixture) = compile_c("events", "event", EVENT_DRIVER_C) else {
        return;
    };
    // The Erlang fixture needs the driver's path (the loader appends the `.so` suffix), which only
    // exists now.
    let source = EVENT_DRIVER_ERL.replace("{path}", &fixture.stem.display().to_string());
    let Some(beam) = compile_erl(&fixture.dir, "event_driver", &source) else {
        fixture.cleanup();
        return;
    };

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(&beam).expect("the event fixture loads");
    let pid = rt
        .spawn_process("event_driver", "recv", vec![])
        .expect("spawn event_driver:recv/0");

    // Drive the scheduler until the receiver returns. The receiver opens the port (whose `start`
    // arms the select and the timer), issues `port_command`, and then blocks in its first
    // `receive`; the same scheduler pass's port half polls the driver's descriptor, runs the
    // scheduled `INPUT` task (⇒ `ready_input` ⇒ `"R"`), delivers it into the mailbox and resumes
    // the receiver; a later pass clamps its poll to the timer's deadline, runs the `TIMEOUT` task
    // (⇒ `timeout` ⇒ `"T"`) and wakes the receiver for the second message. The bounded loop keeps
    // the test independent of the timer's latency rather than asserting a single-call fixpoint.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "the driver port never produced both of its messages"
        );
        rt.run_scheduler().expect("the scheduler pass runs");
        sleep(Duration::from_millis(5));
    }

    // The receiver returned `{Read, Timer}` — the two `{Port, {data, Data}}` payloads in arrival
    // order. Rendered through the runtime's own `~p` printer so a failure names what did arrive
    // (a missing byte is the atom `timeout` in its position).
    let result = rt.process_result(pid).expect("the receiver returned");
    let rendered = rt
        .format_term_w(result, None)
        .unwrap_or_else(|error| format!("<unprintable: {error}>"));
    for (index, byte, label) in [(0, b'R', "ready_input"), (1, b'T', "timeout")] {
        let element = terms::tuple_element(result, index).unwrap_or_else(|_| {
            panic!("expected the {{Read, Timer}} tuple of the driver port, got {rendered}")
        });
        assert_eq!(
            terms::binary_bytes(element).unwrap_or_else(|_| panic!(
                "{label}: expected `{}` as a binary, got {rendered}",
                char::from(byte)
            )),
            &[byte][..],
            "{label}: the callback's `driver_output` byte must reach the owner (got {rendered})"
        );
    }
    // A returning process exits `normal` (its `x(0)` is not its exit reason), leaves the process
    // table and leaves nothing queued behind.
    assert!(rt.process_exit(pid).is_none(), "the receiver exited normally");
    assert!(rt.processes().get(pid).is_none());
    assert_eq!(rt.run_queue_len(), 0);

    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// The other two entry shapes the A.4 `port_command` path must drive: `outputv`, and the term-data
// `erl_drv_output_term` / `erl_drv_send_term` pair
// ---------------------------------------------------------------------------------------------

/// Compiles the committed event fixture in `mode`, prepending the `#define` that selects it.
///
/// The C source is one committed file whose `output`/`outputv` pair is chosen by
/// `EVENT_DRIVER_MODE` (`fixtures/event_driver.c`): mode 0 (the committed default) is the event/timer
/// driver the tests above use, mode 1 declares **only** `outputv`, and modes 2/3 answer through the
/// term-data path. The `driver_name` follows the mode, so each mode is compiled under its own stem —
/// the runtime derives the name from the loaded path's stem, and the loader rejects a mismatch.
fn compile_event_mode(label: &str, mode: u32, stem: &str) -> Option<Fixture> {
    let source = format!("#define EVENT_DRIVER_MODE {mode}\n{EVENT_DRIVER_C}");
    compile_c(label, stem, &source)
}

/// Drives the committed event fixture compiled in `mode` (under `stem`) through the Erlang entry
/// point `fun`, and hands the returned term to `check`.
///
/// Skips — **loudly**, through [`compile_c`]/[`compile_erl`] — when the C compiler, the host
/// `erl_driver.h` or `erlc` is missing, exactly like every other test in this file.
fn with_event_mode(
    label: &str,
    mode: u32,
    stem: &str,
    fun: &str,
    check: impl FnOnce(&mut Runtime, beam_term::prelude::Eterm),
) {
    // The A.4 link seam: the `.so` leaves its `driver_*`/`erl_drv_*` imports undefined, so the test
    // binary must publish them (see `the_binary_exports_the_driver_symbols_a_driver_imports`).
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());

    let Some(fixture) = compile_event_mode(label, mode, stem) else {
        return;
    };
    // The Erlang fixture needs the driver's path (the loader appends the `.so` suffix), which only
    // exists now.
    let source = EVENT_DRIVER_ERL.replace("{path}", &fixture.stem.display().to_string());
    let Some(beam) = compile_erl(&fixture.dir, "event_driver", &source) else {
        fixture.cleanup();
        return;
    };

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(&beam).expect("the event fixture loads");
    let pid = rt
        .spawn_process("event_driver", fun, vec![])
        .unwrap_or_else(|error| panic!("spawn event_driver:{fun}/0: {error:?}"));

    // The same bounded scheduler drive the event/timer test uses: the entry opens the port and
    // blocks in its `receive`, so only a later scheduler pass can deliver what the driver produced.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "event_driver:{fun}/0 never returned (mode {mode})"
        );
        rt.run_scheduler().expect("the scheduler pass runs");
        sleep(Duration::from_millis(5));
    }

    let result = rt.process_result(pid).expect("the receiver returned");
    check(&mut rt, result);

    // A returning process exits `normal` and leaves the table (its `x(0)` is not its exit reason).
    assert!(
        rt.process_exit(pid).is_none(),
        "event_driver:{fun}/0 exited normally"
    );
    fixture.cleanup();
}

/// The **bare** `{7, 9}` the two term-data modes emit: the term itself, never a `{Port, Term}`
/// wrapper (the C queues the decoded term, `io.c:6174`).
///
/// The C spec builds it from a plain `ERL_DRV_INT` (7) and an `ERL_DRV_EXT2TERM` operand carrying the
/// ETF image `<<131, 97, 9>>` (VERSION_MAGIC + the small int 9), so the same assertion also proves
/// the driver-side ETF decoder (`DriverHost::binary_to_term`) was reached **through** the spec
/// decoder. The Erlang half matches the clause `{7, 9}` specifically, so the only other message this
/// port can produce — a `{Port, {data, …}}` wrapper — would not have matched and would have surfaced
/// as the `timeout` atom after 5 s.
fn assert_is_the_7_9_tuple(rt: &mut Runtime, result: beam_term::prelude::Eterm, which: &str) {
    let rendered = rt
        .format_term_w(result, None)
        .unwrap_or_else(|error| format!("<unprintable: {error}>"));
    let Some(size) = terms::tuple_arity(result).ok() else {
        panic!("{which}: the emitted term must reach its recipient as the bare `{{7, 9}}` tuple; \
                got {rendered} (the `timeout` atom means nothing was delivered)");
    };
    assert_eq!(size, 2, "{which}: expected a 2-tuple; got {rendered}");
    let low = terms::tuple_element(result, 0).expect("element 0 of the 2-tuple");
    let high = terms::tuple_element(result, 1).expect("element 1 of the 2-tuple");
    assert!(
        is_small(low) && is_small(high),
        "{which}: both elements must be small ints; got {rendered}"
    );
    assert_eq!(
        (signed_value(low), signed_value(high)),
        (7, 9),
        "{which}: the `ERL_DRV_EXT2TERM` operand must decode to 9 and the `ERL_DRV_INT` to 7; \
         got {rendered}"
    );
}

/// An entry that declares **only** `outputv` (leaving `output` NULL) still receives `port_command`
/// and its `driver_output` reply reaches the owner.
///
/// The C's own preference is the vector callback when a driver declares one
/// (`if (drv->outputv) …` else `… drv->output`, `io.c:1782`), and an entry that declares *only*
/// `outputv` must not be left without an output path. The fixture's `event_outputv` echoes the
/// vector's data block (`iov[1]`) — the vector mirrors the C's build, `iov[0]` being the empty
/// header block — so the bytes the owner receives are evidence of *both* halves: the callback ran
/// (only `port_command` invokes it) and it really saw the payload it echoed.
///
/// Oracle ground truth (OTP 29 / `erts-17.1`, the very same `.so` compiled as `eventv`):
///
/// ```none
/// $ erl -noshell -eval 'ok = erl_ddll:load_driver("<dir>", "eventv"),
///       P = open_port({spawn_driver, "eventv"}, [binary]),
///       true = port_command(P, <<"vv">>),
///       receive {P,{data,D}} -> io:format("~p~n",[D]) after 5000 -> timeout end, halt(0).'
/// <<"vv">>
/// ```
#[test]
fn an_outputv_only_entry_receives_port_command() {
    with_event_mode("outputv", 1, "eventv", "recv_v", |rt, result| {
        let rendered = rt
            .format_term_w(result, None)
            .unwrap_or_else(|error| format!("<unprintable: {error}>"));
        assert_eq!(
            terms::binary_bytes(result).unwrap_or_else(|_| panic!(
                "`outputv` must echo the payload as a binary, got {rendered}"
            )),
            &b"vv"[..],
            "`outputv` must receive `port_command`'s data block and its `driver_output` reply must \
             reach the owner; got {rendered}"
        );
    });
}

/// A term a driver emits with `erl_drv_output_term` is delivered to the port's **owner** as the
/// **term itself**.
///
/// This is the end-to-end proof of the term-data path (contract A.4): `port_command` reaches the
/// fixture's `output`, which decodes a spec holding an `ERL_DRV_EXT2TERM` operand and hands the term
/// to this runtime's [`DriverPort`] host; the port pump drains it as a bare message
/// (`PortEvent::Term { to: None, … }`, the owner) and the scheduler delivers it into the owner's
/// mailbox, which is the only thing that can wake the blocked `receive {7, 9}`.
///
/// Oracle ground truth (OTP 29 / `erts-17.1`, the very same `.so` compiled as `eventt`):
///
/// ```none
/// $ erl -noshell -eval 'ok = erl_ddll:load_driver("<dir>", "eventt"),
///       P = open_port({spawn_driver, "eventt"}, [binary]),
///       true = port_command(P, <<"go">>),
///       receive {7,9} -> io:format("~p~n",[{7,9}]) after 5000 -> timeout end, halt(0).'
/// {7,9}
/// ```
#[test]
fn a_driver_output_term_reaches_the_owner_as_a_bare_term() {
    with_event_mode("outputterm", 2, "eventt", "recv_term", |rt, result| {
        assert_is_the_7_9_tuple(rt, result, "erl_drv_output_term");
    });
}

/// The same term, emitted with `erl_drv_send_term` to `driver_connected`'s pid, reaches that
/// **explicit destination**.
///
/// `erl_drv_send_term`'s `to` operand is the pid **term data** `driver_connected` answers with, so
/// the port pump resolves it through `ProcessId::from_eterm` and delivers to the named pid
/// (`PortEvent::Term { to: Some(pid), … }`) rather than defaulting to the owner — a destination this
/// runtime cannot resolve is dropped, never misdelivered. The Erlang half is the same
/// `recv_term/0`, so the assertion also shows the explicit-destination path is *indistinguishable*
/// from the owner path when the connected process is the recipient, exactly as the oracle has it.
///
/// Oracle ground truth (OTP 29 / `erts-17.1`, the very same `.so` compiled as `eventp`):
///
/// ```none
/// $ erl -noshell -eval 'ok = erl_ddll:load_driver("<dir>", "eventp"),
///       P = open_port({spawn_driver, "eventp"}, [binary]),
///       true = port_command(P, <<"go">>),
///       receive {7,9} -> io:format("~p~n",[{7,9}]) after 5000 -> timeout end, halt(0).'
/// {7,9}
/// ```
#[test]
fn a_driver_send_term_reaches_the_connected_process() {
    with_event_mode("sendterm", 3, "eventp", "recv_term", |rt, result| {
        assert_is_the_7_9_tuple(rt, result, "erl_drv_send_term");
    });
}

// ---------------------------------------------------------------------------------------------
// The split reply (`driver_output2`): the header/body boundary is observable in Erlang
// ---------------------------------------------------------------------------------------------
/// A driver that answers `port_command` through `driver_output2` keeps the C's **header/body**
/// boundary: the owner receives `[72,68,82|<<"abc">>]`, never the flattened `<<"HDRabc">>`.
///
/// This is the end-to-end proof that the host seam's split hook is adopted. `driver_output2(hbuf,
/// hlen, buf, len)` (`io.c:6387`; `driver_output_binary`/`driver_outputv` have the same shape) does
/// not hand up one byte string — it builds `{Port, {data, Reply}}` with `Reply = [h…|Body]`, the
/// header as a **list prefix** and the body as the tail, which `[binary]` port mode renders as a
/// binary. Joining the halves would deliver a *different term*, so this test names both halves and
/// their order.
///
/// Oracle ground truth (OTP 29 / `erts-17.1`, the very same `.so` compiled as `eventsplit`):
///
/// ```none
/// $ erl -noshell -eval 'ok = erl_ddll:load_driver("<dir>", "eventsplit"),
///       P = open_port({spawn_driver, "eventsplit"}, [binary]),
///       true = port_command(P, <<"abc">>),
///       receive {P,{data,D}} -> io:format("~p~n",[D]) after 5000 -> timeout end, halt(0).'
/// [72,68,82|<<"abc">>]
/// ```
#[test]
fn a_driver_output2_reply_keeps_its_header_and_body_apart() {
    with_event_mode("split", 4, "eventsplit", "recv_split", |rt, result| {
        let rendered = rt
            .format_term_w(result, None)
            .unwrap_or_else(|error| format!("<unprintable: {error}>"));
        assert_eq!(
            rendered.trim_end(), "[72,68,82|<<97,98,99>>]",
            "`driver_output2` must deliver the header as a list prefix and the body as the binary \
             tail (the flattened `<<72,68,82,97,98,99>>` is a DIFFERENT term, so a join is a \
             divergence, not a degradation); got {rendered}"
        );
    });
}
// ---------------------------------------------------------------------------------------------
// The driver registry surface: `driver_lock_driver` + `add_driver_entry`/`remove_driver_entry`
// ---------------------------------------------------------------------------------------------

/// The **driver-registry** C fixture, compiled at test time by [`compile_c`] under the stem `reg`
/// (the `driver_name` its entry declares).
///
/// Its `start` callback exercises the four port-less registry slots — `driver_lock_driver`,
/// `add_driver_entry`, and `remove_driver_entry` twice (the helper entry it just added, then the
/// port's **own** entry) — and stashes the four codes in its private state. `output` prints them as
/// `lock,add,remove,remove_own`, so the bytes the owner receives are direct evidence of all four
/// hooks having run against this runtime's host. The second `remove` is `0` (the entry is already
/// gone) and the third is `-1` (the port's own driver was `dlopen`ed, so the C refuses it).
///
/// The oracle answers `0,1,0,-1` for the same `.so` (see
/// [`driver_lock_driver_and_the_entry_registry_agree_with_the_oracle`]).
const REG_DRIVER_C: &str = r#"
#include <erl_driver.h>
#include <stdio.h>

typedef struct { ErlDrvPort port; int c_lock, c_add, c_add2, c_dyn; } reg_state;

static ErlDrvEntry reg_helper_entry;
static ErlDrvEntry reg_driver_entry;

static ErlDrvData reg_start(ErlDrvPort port, char *command) {
    reg_state *st = (reg_state *) driver_alloc(sizeof(reg_state));
    (void) command;
    if (st == NULL) return (ErlDrvData) -1;
    st->port = port;
    st->c_lock = driver_lock_driver(port);
    add_driver_entry(&reg_helper_entry);
    st->c_add = remove_driver_entry(&reg_helper_entry);
    st->c_add2 = remove_driver_entry(&reg_helper_entry);
    st->c_dyn = remove_driver_entry(&reg_driver_entry);
    return (ErlDrvData) st;
}
static void reg_stop(ErlDrvData d) { if (d) driver_free(d); }
static void reg_output(ErlDrvData d, char *buf, ErlDrvSizeT len) {
    reg_state *st = (reg_state *) d;
    char out[64]; int n;
    (void) buf; (void) len;
    if (st == NULL) return;
    n = snprintf(out, sizeof(out), "%d,%d,%d,%d", st->c_lock, st->c_add, st->c_add2, st->c_dyn);
    (void) driver_output(st->port, out, (ErlDrvSizeT) n);
}
static ErlDrvEntry reg_helper_entry = {
    .init = NULL, .driver_name = "reg_helper",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};
static ErlDrvEntry reg_driver_entry = {
    .init = NULL, .start = reg_start, .stop = reg_stop, .output = reg_output,
    .driver_name = "reg",
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};
DRIVER_INIT(reg) { return &reg_driver_entry; }
"#;

/// The Erlang half of the registry test: open the `reg` driver port, `port_command` it, and return
/// the four codes its `output` callback printed.
///
/// `{path}` is replaced by the compiled driver's path (without the `.so` suffix) at test time —
/// this runtime's Milestone-1 `{spawn_driver, Path}` form (see [`beam_rt::port::driver_lookup`]).
const REG_RECV_ERL: &str = r#"%% Generated by `beam_rt/tests/driver.rs` (never committed).
-module(reg_recv).
-export([probe/0]).

probe() ->
    Port = open_port({spawn_driver, "{path}"}, [binary]),
    true = port_command(Port, <<"go">>),
    receive {Port, {data, D}} -> D end.
"#;

/// **`driver_lock_driver` + `add_driver_entry`/`remove_driver_entry` agree with the C**, driven
/// through the **real binary**'s `run` action.
///
/// The four port-less registry slots (`io.c:7324`/`:7640`/`:7698`, plus `driver_create_port`) are
/// the ones an A.4 driver reaches with **no term services of their own**: the frozen `driver_*`
/// table publishes them as free functions that recover the current host from the thread-local
/// publication [`beam_rt::port::driver`]'s `publishing` installs around every callback. This test
/// proves the whole path — the C calls the hooks, they reach [`beam_rt::port::driver::DriverPort`],
/// and it answers the C's own codes:
///
/// * `driver_lock_driver` = **0** (the port's driver was `dlopen`ed, so `prt->drv_ptr->handle` is
///   non-null, `io.c:7337`);
/// * `add_driver_entry` (a statically linked helper) then `remove_driver_entry` = **1** (recorded,
///   then found and removed);
/// * a second `remove` of the same helper = **0** (no such entry);
/// * a `remove` of the port's **own** entry = **-1** (it was recorded as dynamically loaded at
///   `DriverPortAdapter::load`, so the C refuses it, `io.c:7709-7713`).
///
/// The bytes really cross the A.4 boundary: the driver writes them with `driver_output`, the
/// runtime captures them into the port's input, and the scheduler delivers `{Port, {data, Data}}`
/// to the port's owner — which is the only thing that can wake the blocked `receive`.
///
/// `run` renders its result with `~w~n`, so the driver's raw bytes `{"0,1,0,-1"}` appear as the
/// binary's byte list `<<48,44,49,44,48,44,45,49>>` (OTP 29's `~w` of a binary is always the integer
/// form — `erl -noshell -eval 'io:format("~w~n",[<<"0,1,0,-1">>])'` prints exactly that). The
/// oracle below prints the same four codes raw with `~s`, so both sides pin the identical bytes.
#[test]
fn driver_lock_driver_and_the_entry_registry_agree_with_the_oracle() {
    // The A.4 link seam: the `.so` leaves its `driver_*` imports undefined, so the binary must
    // publish them (see `the_binary_exports_the_driver_symbols_a_driver_imports`).
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());

    let Some(fixture) = compile_c("registry", "reg", REG_DRIVER_C) else {
        return;
    };
    let source = REG_RECV_ERL.replace("{path}", &fixture.stem.display().to_string());
    let Some(beam) = compile_erl(&fixture.dir, "reg_recv", &source) else {
        fixture.cleanup();
        return;
    };

    // --- the runtime's own answer, through the real binary (`run` drives the full scheduler) ----
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args([
            "-noshell".to_string(),
            "-pa".to_string(),
            fixture.dir.display().to_string(),
            "run".to_string(),
            beam.display().to_string(),
            "reg_recv:probe/0".to_string(),
        ])
        .output()
        .expect("the beam_rt binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(0),
        "the driver registry round-trip must complete under `run`: stderr={stderr}"
    );
    // The driver's four codes, as `run`'s `~w~n` renders the returned binary.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "<<48,44,49,44,48,44,45,49>>\n",
        "lock=0, add+remove=1, second remove=0, own-entry remove=-1; stderr={stderr}"
    );
    assert!(output.stderr.is_empty(), "stderr: {stderr}");

    // --- the oracle cross-check: the real system `erl` answers the same four codes -------------
    if which_erl().is_none() {
        skip("driver/registry: no `erl` on PATH for the oracle cross-check");
        fixture.cleanup();
        return;
    }
    let dir = fixture.dir.display().to_string();
    let eval = format!(
        "ok = erl_ddll:load_driver(\"{dir}\", \"reg\"), \
         Port = open_port({{spawn_driver, \"reg\"}}, [binary]), \
         true = port_command(Port, <<\"go\">>), \
         receive {{Port, {{data, D}}}} -> io:format(\"~s\",[D]) after 3000 -> io:format(\"TIMEOUT\") end, \
         halt(0)."
    );
    let oracle = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .expect("the oracle `erl` runs");
    assert!(
        oracle.status.success(),
        "the oracle run failed: status={} stderr={}",
        oracle.status,
        String::from_utf8_lossy(&oracle.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&oracle.stdout),
        "0,1,0,-1",
        "the oracle's own four codes are the reference; stderr={}",
        String::from_utf8_lossy(&oracle.stderr)
    );

    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// The CLI `run` action drives the scheduler (the A.4 echo round-trip end to end)
// ---------------------------------------------------------------------------------------------

/// The Erlang half of the **`run`-action echo** test: open a real C driver port, `port_command`
/// it and return the bytes its owner receives.
///
/// `{path}` is replaced by the compiled driver's path (without the `.so` suffix) at test time —
/// this runtime's Milestone-1 `{spawn_driver, Path}` form (see `beam_rt::port::driver_lookup`).
const DRV_RUN_ECHO_ERL: &str = r#"%% Generated by `beam_rt/tests/driver.rs` (never committed).
-module(drv_run_echo).
-export([echo/0]).

echo() ->
    Port = open_port({spawn_driver, "{path}"}, [binary]),
    true = port_command(Port, <<"hello">>),
    receive {Port, {data, D}} -> D end.
"#;

/// **The CLI's `run` action drives the full scheduler**: a driver echo round-trip really
/// completes under `run`.
///
/// `run` loads the file, spawns the export as a real process and pumps the scheduler until it
/// terminates, so the receiver blocked on `{Port, {data, Data}}` is served by the port half of
/// a **later** scheduler pass and the driver's echo reaches the owner.
///
/// Driven through the **real binary** (`CARGO_BIN_EXE_beam_rt`), because the C driver leaves
/// its `driver_*` symbols undefined and `dlopen(RTLD_NOW)` resolves them from the loading
/// process's dynamic symbol table — the `-Wl,--export-dynamic` link arg `build.rs` emits for
/// the binary, together with [`PortRegistry::new`](beam_rt::port::PortRegistry)'s anchor of
/// `beam_abi`'s driver codegen unit.
///
/// Oracle: the system `erl` echoes `<<"hello">>` for this same `.so`
/// (`tests/fixtures/README.md`'s recipe; `~p` renders the printable form). `run` renders its
/// result with `~w~n` — the oracle's own rendering for a module-run case — and erts-17.1's
/// `~w` of `<<"hello">>` is `<<104,101,108,108,111>>` (verified:
/// `erl -noshell -eval 'io:format("~w~n",[<<"hello">>]).'`), so the asserted line below is
/// byte-identical to the oracle for the very same bytes.
#[test]
fn the_run_action_drives_the_scheduler_and_echoes_a_driver_port() {
    let Some(fixture) = build_fixture("run_echo") else {
        return;
    };
    let source = DRV_RUN_ECHO_ERL.replace("{path}", &fixture.stem.display().to_string());
    let Some(beam) = compile_erl(&fixture.dir, "drv_run_echo", &source) else {
        fixture.cleanup();
        return;
    };

    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args([
            "-noshell",
            "run",
            &beam.display().to_string(),
            "drv_run_echo:echo/0",
        ])
        .output()
        .expect("the beam_rt binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(0),
        "the echo round-trip must complete under `run`: stderr={stderr}"
    );
    // `<<"hello">>` rendered by `~w`: the driver's echoed bytes really are the result.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "<<104,101,108,108,111>>\n",
        "the driver's echoed bytes must be the printed result; stderr={stderr}"
    );
    assert!(output.stderr.is_empty(), "stderr: {stderr}");

    fixture.cleanup();
}
