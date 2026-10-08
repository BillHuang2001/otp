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

#![allow(dead_code)]
//! Shared helpers for the `beam_io` integration tests.
//!
//! This module lives in a **subdirectory** of `tests/` on purpose: Cargo treats every `tests/*.rs`
//! file as its own integration-test binary, but a `tests/<dir>/mod.rs` is *not* a target — it is a
//! plain module the test files pull in with `mod support;`. That keeps the helpers compiled and
//! warning-free in each including binary without spawning a spurious "0 tests" target.
//!
//! The `#![allow(dead_code)]` above is required: the module is compiled into *every* integration
//! test binary that includes it, so a helper unused by one binary would otherwise trip the
//! `dead_code` lint there (same convention as `beam_abi/tests/common/mod.rs`).
//!
//! # Skip-vs-fail policy (explicit)
//!
//! Building the C driver fixture needs host facilities a bare container may lack: a C compiler and
//! the system OTP include directory (which holds `erl_driver.h`). The helpers below report those
//! gaps instead of panicking — [`erl_include_dir`] and [`cc_available`] return `None`/`false`, and
//! the calling test prints a **loud** [`skip`] banner and returns. A skip is printed, never silent.
//!
//! # Why this module carries a `dlopen` probe
//!
//! The crate-level loader for a driver shared object is `beam_abi`'s
//! `beam_abi::driver::loader::load(path, name)` (contract A.4). `beam_abi` is **not** a dependency
//! of `beam_io` on this branch (the sibling task that adds it owns `Cargo.toml`), so a hard
//! reference to it here would be a *compile error* — unacceptable for a test helper. To keep the
//! fixture load-proof meaningful today, [`probe_driver_name`] performs the same, minimal
//! `dlopen(RTLD_NOW)` + `dlsym("driver_init")` + `ErlDrvEntry` read that the crate-level loader
//! does, using only `libc` (already a `beam_io` dependency). Once `beam_abi` is a `beam_io`
//! dependency this probe can be replaced by `beam_abi::driver::loader::load(&path, "host_driver")`
//! followed by `DriverLibrary::name()`; the assertions are the same.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use libc::{c_char, c_void};

/// The header the include-directory search keys on. `erl_driver.h` ships in the **same** directory
/// as `erl_nif.h`, so one probe resolves the include directory for both ABIs.
const HEADER: &str = "erl_driver.h";

// ---------------------------------------------------------------------------------------------
// Locating the system OTP include directory (`erl_driver.h` + `erl_nif.h`)
// ---------------------------------------------------------------------------------------------

/// Locate the system OTP include directory (the one holding `erl_driver.h`); `None` if unavailable.
///
/// Resolution order (the first directory that actually contains a readable `erl_driver.h` wins):
///
/// 1. ask the system Erlang: `erl -noshell -eval 'io:format("~s",[code:lib_dir(erts,include)])'`.
///    Some packaged runtimes (notably the Nix build) answer with a path that does not exist, so the
///    answer is accepted only if `<dir>/erl_driver.h` really is there;
/// 2. resolve the `erl` binary found on `PATH` and canonicalize it: an installed runtime ships the
///    real binary at `.../erts-<vsn>/bin/erl`, whose sibling `.../erts-<vsn>/include` holds it;
/// 3. walk a small set of standard installation roots (`$ERL_TOP`, `/usr/lib/erlang`, …), checking
///    `<root>/include` and any `erts-*` directory directly under the root or its `lib/erlang`.
pub fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval()
        .or_else(from_path_erl)
        .or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}

/// True when `dir` holds a readable `erl_driver.h`.
fn has_header(dir: &Path) -> bool {
    dir.join(HEADER).is_file()
}

/// Step 1: ask the system `erl` for `code:lib_dir(erts, include)`.
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

/// Step 2: canonicalize the `erl` on `PATH` and take the sibling `include` of its `erts-*` dir.
fn from_path_erl() -> Option<PathBuf> {
    let erl = which_erl()?.canonicalize().ok()?;
    // `.../erts-<vsn>/bin/erl` -> `.../erts-<vsn>/include`
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    if !erts.file_name()?.to_string_lossy().starts_with("erts-") {
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

/// Step 3: the standard installation roots to probe.
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
// Compiling a driver shared object
// ---------------------------------------------------------------------------------------------

/// Is a C compiler available? (uses `$CC` if set, else `"cc"`)
pub fn cc_available() -> bool {
    Command::new(cc())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Compile the C source `src` into the shared object `out`, using `include` as the OTP include
/// directory.
///
/// Runs `cc -shared -fPIC -I<include> -o <out> <src>`; on any failure (a missing compiler, a
/// compiler error, or no output file) it prints the compiler's stderr and returns `false`. It never
/// panics, so a caller can distinguish "the toolchain is unavailable" (checked before the call with
/// [`cc_available`]/[`erl_include_dir`]) from "the fixture did not build" (an assertion).
pub fn compile_so(src: &Path, out: &Path, include: &Path) -> bool {
    if let Some(parent) = out.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!("compile_so: cannot create {}: {err}", parent.display());
            return false;
        }
    }
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(out)
        .arg(src)
        .output();
    match output {
        Ok(output) => {
            if !output.status.success() {
                eprintln!(
                    "compile_so: cc failed (exit {:?}):\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                );
                return false;
            }
            out.is_file()
        }
        Err(err) => {
            eprintln!("compile_so: could not run the C compiler: {err}");
            false
        }
    }
}

/// A **fresh** scratch directory under [`std::env::temp_dir`] (`$TMPDIR`), unique per stem +
/// process + call, so parallel test binaries do not collide.
///
/// Freshness is part of the contract, not a nicety: a test compiles an object here and the C
/// fixture *appends* its callback log (`fopen(..., "a")`), so a directory that still holds a
/// previous run's file makes an assertion read stale lines. The name therefore mixes the process
/// id, a per-binary call counter and the wall clock, and any directory already at the resulting
/// path is removed first — guaranteeing an empty directory even in the (now vanishingly unlikely)
/// case of a recycled process id landing on the same `(stem, counter)` slot.
pub fn scratch_dir(stem: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "beam_io-{stem}-{}-{n}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Print a loud skip banner (never a silent pass), test-name + reason.
pub fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// ---------------------------------------------------------------------------------------------
// A minimal `dlopen` probe for a driver shared object
// ---------------------------------------------------------------------------------------------

/// The prefix of the frozen `ErlDrvEntry` (`erl_driver.h`) up to and including `driver_name`.
///
/// `driver_name` is the 7th field of the struct and every field before it is a pointer, so a
/// 7-pointer `#[repr(C)]` prefix reproduces the real offsets on any target — enough to read the
/// loaded driver's name without depending on `beam_abi`'s full layout.
#[repr(C)]
struct ErlDrvEntryPrefix {
    init: *mut c_void,
    start: *mut c_void,
    stop: *mut c_void,
    output: *mut c_void,
    ready_input: *mut c_void,
    ready_output: *mut c_void,
    driver_name: *mut c_char,
}

/// The shape of the `driver_init` export (`DRIVER_INIT(name)` in `erl_driver.h`).
type DriverInitFn = unsafe extern "C" fn() -> *mut ErlDrvEntryPrefix;

/// The exported symbol `DRIVER_INIT` produces.
const DRIVER_INIT_SYMBOL: &[u8] = b"driver_init\0";

/// `dlopen` `path` (`RTLD_NOW`), call its `driver_init` export and return the loaded driver's
/// `driver_name`.
///
/// * `Ok(Some(name))` — the object loaded, `driver_init` returned an entry and it carries a name.
/// * `Ok(None)` — the object loaded but `driver_init` returned `NULL`.
/// * `Err(text)` — the object could not be loaded (typically an **unresolved `driver_*` symbol**,
///   i.e. the loading process does not export the host functions / `-Wl,--export-dynamic` is not
///   wired), or it has no `driver_init`.
///
/// This mirrors the load half of `beam_abi::driver::loader::load` (see the module docs for why the
/// real loader is not called here). It never panics and always closes the handle it opened.
pub fn probe_driver_name(path: &Path) -> Result<Option<String>, String> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path contains a NUL byte: {}", path.display()))?;

    // SAFETY: `c_path` is a valid NUL-terminated path; `RTLD_NOW` resolves all undefined symbols
    // eagerly. `dlopen` returns NULL on failure (reported through `dl_error`) or an owned handle.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW) };
    if handle.is_null() {
        return Err(dl_error());
    }

    // SAFETY: `handle` is a live object from `dlopen`; the symbol name is NUL-terminated.
    let sym = unsafe { libc::dlsym(handle, DRIVER_INIT_SYMBOL.as_ptr() as *const c_char) };
    if sym.is_null() {
        let err = dl_error();
        // SAFETY: `handle` came from `dlopen` and is not used afterwards.
        unsafe { libc::dlclose(handle) };
        return Err(format!("no driver_init symbol: {err}"));
    }

    // SAFETY: `sym` is the address of `ErlDrvEntry *driver_init(void)` (what `DRIVER_INIT`
    // exports); a raw pointer and a function pointer have the same size, and the signature matches
    // the header's declaration.
    let init: DriverInitFn = unsafe { std::mem::transmute(sym) };
    // SAFETY: `init` is the driver's `driver_init`, callable with no arguments.
    let entry = unsafe { init() };
    if entry.is_null() {
        // SAFETY: `handle` came from `dlopen` and the entry is not used afterwards.
        unsafe { libc::dlclose(handle) };
        return Ok(None);
    }

    // SAFETY: `entry` is the packed `ErlDrvEntry` `driver_init` returned; the prefix struct reads
    // its first seven pointer fields, of which `driver_name` is the seventh.
    let name_ptr = unsafe { (*entry).driver_name };
    let name = if name_ptr.is_null() {
        None
    } else {
        // SAFETY: `driver_name` is a NUL-terminated C string per the driver ABI; the bytes are
        // copied into an owned `String` before the handle is closed.
        Some(unsafe { CStr::from_ptr(name_ptr) }.to_string_lossy().into_owned())
    };
    // SAFETY: `handle` came from `dlopen` and is not used afterwards.
    unsafe { libc::dlclose(handle) };
    Ok(name)
}

/// The dynamic loader's current error text, or a placeholder when it reports none.
fn dl_error() -> String {
    // SAFETY: `dlerror` returns either NULL or a pointer to a NUL-terminated string owned by the
    // loader; it also clears the loader's error state, which is why it is called once here.
    let ptr = unsafe { libc::dlerror() };
    if ptr.is_null() {
        "unknown dynamic-loader error".to_string()
    } else {
        // SAFETY: `ptr` is a valid NUL-terminated string per `dlerror`'s contract.
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
    }
}
