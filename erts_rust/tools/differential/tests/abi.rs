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

//! Contract **A.3** (NIF C ABI) and **A.4** (driver C ABI) — the structural ABI check.
//!
//! The `enif_*` / `driver_*` / `erl_drv_*` C ABIs are **not** observable through the
//! black-box runtime CLI (a NIF/driver is C code linked into the emulator), so they cannot
//! be exercised by a differential corpus case. What *can* be checked is that the frozen
//! declarations still form a compilable interface: this test compiles a C translation unit
//! that merely **references** the ABI surface against the in-tree headers, which is exactly
//! what a NIF/driver author does.
//!
//! It deliberately shells out to the system C compiler rather than depending on any
//! `crates/*` crate (the harness has no coupling to the runtime's internals). When the
//! compiler, the headers or the target's integer sizes are not discoverable the test
//! **skips loudly** with a printed reason — it never fabricates a pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

/// The `enif_*` symbols the structural check references (a representative slice of the
/// NIF ABI: allocation, term construction/access, binaries, lists/maps, resources,
/// comparison, string/term marshalling, messaging, monitors, I/O, threads and
/// synchronisation). Every name is declared in `erl_nif_api_funcs.h`/`erl_nif.h`.
const NIF_SYMBOLS: &[&str] = &[
    // Allocation.
    "enif_alloc",
    "enif_free",
    // Atoms, integers and type tests.
    "enif_make_atom",
    "enif_make_int",
    "enif_get_int",
    "enif_make_int64",
    "enif_get_int64",
    "enif_make_uint64",
    "enif_get_uint64",
    "enif_make_double",
    "enif_get_double",
    "enif_is_atom",
    "enif_is_list",
    "enif_is_map",
    // Tuples.
    "enif_make_tuple",
    "enif_get_tuple",
    // Binaries.
    "enif_alloc_binary",
    "enif_make_new_binary",
    "enif_release_binary",
    "enif_inspect_binary",
    "enif_make_sub_binary",
    "enif_make_resource_binary",
    // Lists and maps.
    "enif_make_list",
    "enif_make_list_cell",
    "enif_get_list_cell",
    "enif_make_list_from_array",
    "enif_make_map_put",
    "enif_get_map_value",
    "enif_make_map_from_arrays",
    "enif_get_map_size",
    // Comparison, references and resources.
    "enif_is_identical",
    "enif_compare",
    "enif_make_ref",
    "enif_make_resource",
    "enif_open_resource_type",
    "enif_priv_data",
    // Strings and the term<->binary codec.
    "enif_make_string",
    "enif_get_string",
    "enif_term_to_binary",
    "enif_binary_to_term",
    // Messaging, monitors, I/O, threads and synchronisation.
    "enif_send",
    "enif_monitor_process",
    "enif_select",
    "enif_thread_create",
    "enif_mutex_create",
    "enif_cond_create",
    "enif_rwlock_create",
    "enif_tsd_key_create",
    "enif_dlopen",
];

/// The `driver_*` / `erl_drv_*` symbols the structural check references (the driver ABI:
/// allocation, output, the port output queue, select/async, term marshalling, monitors,
/// threads/synchronisation). Every name is declared in `erl_driver.h`.
const DRIVER_SYMBOLS: &[&str] = &[
    // Allocation.
    "driver_alloc",
    "driver_free",
    "driver_alloc_binary",
    "driver_binary_get_refc",
    "driver_binary_inc_refc",
    "driver_binary_dec_refc",
    // Output and the port output queue.
    "driver_output",
    "driver_output2",
    "driver_output_binary",
    "driver_outputv",
    "driver_enq",
    "driver_deq",
    "driver_sizeq",
    "driver_enq_bin",
    "driver_peekq",
    "driver_peekqv",
    "driver_enqv",
    // Select, async and termination.
    "driver_select",
    "driver_async",
    "driver_exit",
    // Term marshalling and ports.
    "driver_mk_atom",
    "driver_mk_port",
    "driver_output_term",
    "erl_drv_output_term",
    "erl_drv_send_term",
    "driver_compare_monitors",
    "erl_drv_busy_msgq_limits",
    // Threads and synchronisation.
    "erl_drv_thread_create",
    "erl_drv_mutex_create",
    "erl_drv_mutex_lock",
    "erl_drv_mutex_unlock",
    "erl_drv_mutex_destroy",
    "erl_drv_rwlock_create",
    "erl_drv_cond_create",
    "erl_drv_tsd_key_create",
];

/// The frozen types the two ABIs expose (`ErlNifEnv` is opaque by design, so it is only
/// referenced through a pointer). A `sizeof` is asserted for every one.
const ABI_TYPES: &[&str] = &[
    "ErlNifPid",
    "ErlNifPort",
    "ErlNifBinary",
    "ErlNifFunc",
    "ErlNifEntry",
    "ErlNifEnv",
    "ErlNifResourceTypeInit",
    "ErlNifMapIterator",
    "ErlNifIOVec",
    "ErlDrvBinary",
    "ErlDrvEntry",
    "ErlIOVec",
    "ErlDrvMonitor",
    "ErlDrvSysInfo",
    "ErlDrvThreadOpts",
    "SysIOVec",
];

/// The frozen size/align/offset numbers of the two ABIs (`crates/beam_abi/src/layout.rs`
/// is the Rust freeze) mirrored as C `_Static_assert`s. Every number below is one the
/// in-tree headers can express (`sizeof`, `_Alignof`, `offsetof`), so a header change that
/// breaks the C ABI becomes a **compile failure** in this check, not a silent drift.
const ABI_STATIC_ASSERTS: &[&str] = &[
    // The term seam: the NIF term is one machine word (contract A.3).
    "_Static_assert(sizeof(ERL_NIF_TERM) == 8, \"ERL_NIF_TERM is one machine word\");",
    "_Static_assert(_Alignof(ERL_NIF_TERM) == 8, \"ERL_NIF_TERM alignment\");",
    // ErlNifFunc / ErlNifEntry.
    "_Static_assert(sizeof(ErlNifFunc) == 32, \"ErlNifFunc size\");",
    "_Static_assert(offsetof(ErlNifFunc, name) == 0, \"ErlNifFunc.name\");",
    "_Static_assert(offsetof(ErlNifFunc, arity) == 8, \"ErlNifFunc.arity\");",
    "_Static_assert(offsetof(ErlNifFunc, fptr) == 16, \"ErlNifFunc.fptr\");",
    "_Static_assert(offsetof(ErlNifFunc, flags) == 24, \"ErlNifFunc.flags\");",
    "_Static_assert(sizeof(ErlNifEntry) == 96, \"ErlNifEntry size\");",
    "_Static_assert(offsetof(ErlNifEntry, funcs) == 24, \"ErlNifEntry.funcs\");",
    "_Static_assert(offsetof(ErlNifEntry, sizeof_ErlNifResourceTypeInit) == 80, \"ErlNifEntry.sizeof_ErlNifResourceTypeInit\");",
    "_Static_assert(offsetof(ErlNifEntry, min_erts) == 88, \"ErlNifEntry.min_erts\");",
    // ErlNifBinary.
    "_Static_assert(sizeof(ErlNifBinary) == 40, \"ErlNifBinary size\");",
    "_Static_assert(offsetof(ErlNifBinary, data) == 8, \"ErlNifBinary.data\");",
    "_Static_assert(offsetof(ErlNifBinary, ref_bin) == 16, \"ErlNifBinary.ref_bin\");",
    "_Static_assert(offsetof(ErlNifBinary, __spare__) == 24, \"ErlNifBinary.__spare__\");",
    // Pid/port handles.
    "_Static_assert(sizeof(ErlNifPid) == 8, \"ErlNifPid size\");",
    "_Static_assert(sizeof(ErlNifPort) == 8, \"ErlNifPort size\");",
    // ErlNifResourceTypeInit.
    "_Static_assert(sizeof(ErlNifResourceTypeInit) == 40, \"ErlNifResourceTypeInit size\");",
    "_Static_assert(offsetof(ErlNifResourceTypeInit, dyncall) == 32, \"ErlNifResourceTypeInit.dyncall\");",
    // ErlNifMapIterator / ErlNifIOVec.
    "_Static_assert(sizeof(ErlNifMapIterator) == 56, \"ErlNifMapIterator size\");",
    "_Static_assert(sizeof(ErlNifIOVec) == 424, \"ErlNifIOVec size\");",
    // Shared structs (`erl_drv_nif.h`).
    "_Static_assert(sizeof(ErlDrvSysInfo) == 56, \"ErlDrvSysInfo size\");",
    "_Static_assert(offsetof(ErlDrvSysInfo, nif_major_version) == 40, \"ErlDrvSysInfo.nif_major_version\");",
    "_Static_assert(offsetof(ErlDrvSysInfo, dirty_scheduler_support) == 48, \"ErlDrvSysInfo.dirty_scheduler_support\");",
    "_Static_assert(sizeof(ErlDrvThreadOpts) == 4, \"ErlDrvThreadOpts size\");",
    "_Static_assert(sizeof(ErlDrvMonitor) == 32, \"ErlDrvMonitor size\");",
    "_Static_assert(_Alignof(ErlDrvMonitor) == 1, \"ErlDrvMonitor alignment\");",
    "_Static_assert(sizeof(SysIOVec) == 16, \"SysIOVec size\");",
    "_Static_assert(offsetof(SysIOVec, iov_len) == 8, \"SysIOVec.iov_len\");",
    // Driver structs (`erl_driver.h`).
    "_Static_assert(sizeof(ErlDrvBinary) == 16, \"ErlDrvBinary size\");",
    "_Static_assert(offsetof(ErlDrvBinary, orig_bytes) == 8, \"ErlDrvBinary.orig_bytes\");",
    "_Static_assert(sizeof(ErlIOVec) == 32, \"ErlIOVec size\");",
    "_Static_assert(sizeof(ErlDrvEntry) == 176, \"ErlDrvEntry size\");",
    "_Static_assert(offsetof(ErlDrvEntry, driver_name) == 48, \"ErlDrvEntry.driver_name\");",
    "_Static_assert(offsetof(ErlDrvEntry, extended_marker) == 128, \"ErlDrvEntry.extended_marker\");",
    "_Static_assert(offsetof(ErlDrvEntry, emergency_close) == 168, \"ErlDrvEntry.emergency_close\");",
];

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> std::io::Result<TempDir> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Under the worktree-local scratch root (see `common::scratch_root`), not the
        // shared/volatile `$TMPDIR` that a sibling worker may clean out from under us.
        let dir = common::scratch_root().join(format!(
            "differential-abi-{}-{}-{n}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The repository root (`erts_rust/..`), derived from the crate manifest dir.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// The in-tree emulator include directory that holds `erl_nif.h` / `erl_driver.h`.
fn beam_include_dir() -> PathBuf {
    repo_root().join("erts/emulator/beam")
}

/// Find an executable on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Resolve a C compiler: `$CC` when set (and usable), else `cc`, `gcc`, `clang` on `PATH`.
fn resolve_cc() -> Option<PathBuf> {
    if let Some(cc) = std::env::var_os("CC") {
        let cc = PathBuf::from(cc);
        return which(cc.to_str().unwrap_or("")).or(Some(cc));
    }
    ["cc", "gcc", "clang"].iter().find_map(|name| which(name))
}

/// Run a command and return `(success, stdout, stderr)`.
fn run(program: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(program).args(args).output();
    match output {
        Ok(output) => (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Err(err) => (false, String::new(), format!("failed to spawn: {err}")),
    }
}

/// Read the target's integer sizes from the compiler's predefined macros.
///
/// `erl_int_sizes_config.h` is a **generated** file (`erts/include/erl_int_sizes_config.h.in`
/// is the template); no generated copy is committed, so the check synthesizes one from
/// `__SIZEOF_*__` exactly as `configure` would.
fn int_sizes_for(cc: &Path) -> Option<String> {
    let (ok, stdout, stderr) = run(cc, &["-dM", "-E", "-x", "c", "/dev/null"]);
    if !ok {
        eprintln!("SKIP: `{} -dM -E` failed: {stderr}", cc.display());
        return None;
    }
    let mut sizes = std::collections::BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("#define __SIZEOF_") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        if let (Some(name), Some(value)) = (parts.next(), parts.next()) {
            let name = name.trim_end_matches("__");
            if let Ok(value) = value.parse::<u32>() {
                sizes.insert(name.to_string(), value);
            }
        }
    }
    // `__SIZEOF_CHAR__` is not emitted by every compiler; a char is one byte.
    let mut header = String::from("/* synthesized by tests/abi.rs (mirrors configure) */\n");
    for (key, defined) in [
        ("CHAR", "CHAR"),
        ("SHORT", "SHORT"),
        ("INT", "INT"),
        ("LONG", "LONG"),
        ("LONG_LONG", "LONG_LONG"),
        ("VOID_P", "VOID_P"),
    ] {
        let Some(value) = macro_size(&sizes, key) else {
            eprintln!("SKIP: the compiler does not define __SIZEOF_{key}__");
            return None;
        };
        header.push_str(&format!("#define SIZEOF_{defined} {value}\n"));
    }
    Some(header)
}

/// The byte size `configure` needs for the `SIZEOF_<key>` macro.
///
/// `configure` names these macros `SIZEOF_CHAR`/`SIZEOF_SHORT`/…/`SIZEOF_VOID_P`; the compiler's
/// predefined `__SIZEOF_*__` macros use the same names **except** for the pointer, which GCC and
/// Clang spell `__SIZEOF_POINTER__` (there is no `__SIZEOF_VOID_P__`). A `char` is one byte by
/// definition. Without the pointer alias, discovery would always fail and the structural check
/// could only ever skip.
fn macro_size(sizes: &std::collections::BTreeMap<String, u32>, key: &str) -> Option<u32> {
    match key {
        "CHAR" => Some(sizes.get("CHAR").copied().unwrap_or(1)),
        "VOID_P" => sizes
            .get("VOID_P")
            .or_else(|| sizes.get("POINTER"))
            .copied(),
        _ => sizes.get(key).copied(),
    }
}

/// The C translation unit: it only **references** the ABI declarations (address-of, so no
/// call is emitted) plus a set of structural `_Static_assert`s pinning the frozen layout.
fn probe_source() -> String {
    let mut src = String::from(
        "/* Structural A.3/A.4 check: reference the frozen NIF/driver C ABI. */\n\
         #include <stddef.h>\n\
         #include \"erl_nif.h\"\n\
         #include \"erl_driver.h\"\n\n",
    );
    for assert in ABI_STATIC_ASSERTS {
        src.push_str(assert);
        src.push('\n');
    }
    src.push('\n');
    for symbol in NIF_SYMBOLS.iter().chain(DRIVER_SYMBOLS) {
        src.push_str(&format!(
            "static __typeof__(&{symbol}) abi_ref_{symbol} = &{symbol};\n"
        ));
    }
    for ty in ABI_TYPES {
        if *ty == "ErlNifEnv" {
            // Opaque: only a pointer may be formed.
            src.push_str("static ErlNifEnv *abi_opaque_env = 0;\n");
        } else {
            src.push_str(&format!("static {ty} abi_dummy_{ty};\n"));
            src.push_str(&format!("static size_t abi_size_{ty} = sizeof({ty});\n"));
        }
    }
    src.push_str(
        "int main(void) {\n\
         \x20   return (int)(abi_size_ErlNifPid ^ abi_size_ErlNifBinary ^ abi_size_SysIOVec\n\
         \x20       ^ (abi_opaque_env == 0 ? 0u : 1u));\n\
         }\n",
    );
    src
}

/// Compile `source` with `-fsyntax-only` against the frozen headers; returns the outcome.
fn compile_probe(cc: &Path, dir: &Path, source: &str) -> Result<(), String> {
    let c_file = dir.join("abi_check.c");
    std::fs::write(&c_file, source).map_err(|e| format!("failed to write the probe: {e}"))?;
    let include = beam_include_dir();
    let (ok, _stdout, stderr) = run(
        cc,
        &[
            "-std=gnu11",
            "-fsyntax-only",
            "-Wall",
            &format!("-I{}", dir.display()),
            &format!("-I{}", include.display()),
            c_file.to_str().expect("utf-8 path"),
        ],
    );
    if ok {
        Ok(())
    } else {
        Err(stderr)
    }
}

/// Prepare the structural check: a C compiler, a synthesized `erl_int_sizes_config.h` and a
/// scratch directory. `None` means the check cannot run here — every failing prerequisite
/// prints its own **loud** skip reason, so it never silently passes.
fn check_env(tag: &str) -> Option<(PathBuf, TempDir)> {
    let Some(cc) = resolve_cc() else {
        eprintln!(
            "SKIP: no C compiler ($CC / cc / gcc / clang) on PATH — \
             cannot run the A.3/A.4 structural check"
        );
        return None;
    };
    let Some(int_sizes) = int_sizes_for(&cc) else {
        eprintln!("SKIP: the target's integer sizes are not discoverable");
        return None;
    };
    let dir = match TempDir::new(tag) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("SKIP: cannot create a scratch directory: {err}");
            return None;
        }
    };
    if let Err(err) = std::fs::write(dir.path().join("erl_int_sizes_config.h"), int_sizes) {
        eprintln!("SKIP: cannot write the synthesized config header: {err}");
        return None;
    }
    Some((cc, dir))
}

#[test]
fn the_frozen_nif_and_driver_declarations_still_compile() {
    let include = beam_include_dir();
    for header in ["erl_nif.h", "erl_driver.h"] {
        if !include.join(header).is_file() {
            eprintln!(
                "SKIP: {} is missing — the frozen ABI headers are not in the tree",
                include.join(header).display()
            );
            return;
        }
    }
    let Some((cc, dir)) = check_env("ok") else {
        return;
    };
    match compile_probe(&cc, dir.path(), &probe_source()) {
        Ok(()) => {}
        Err(stderr) => panic!(
            "the frozen A.3/A.4 declarations no longer compile with {}:\n{stderr}",
            cc.display()
        ),
    }
}

#[test]
fn the_structural_check_rejects_a_missing_symbol() {
    // Self-testing: the check must actually fail when the ABI surface is broken, so a
    // renamed/removed symbol is a loud failure rather than a silent pass.
    let Some((cc, dir)) = check_env("bogus-symbol") else {
        return;
    };
    let bogus = "#include \"erl_nif.h\"\n\
                 static __typeof__(&enif_this_symbol_does_not_exist) bogus = \n\
                 &enif_this_symbol_does_not_exist;\n";
    assert!(
        compile_probe(&cc, dir.path(), bogus).is_err(),
        "the structural ABI check must fail on a symbol that does not exist"
    );
}

#[test]
fn the_structural_check_rejects_a_wrong_frozen_size() {
    // Self-testing the size/align/offset freeze: a `_Static_assert` that disagrees with the
    // header layout must fail the compile, so ABI drift is loud rather than silent (and the
    // freeze cannot be quietly relaxed by deleting an assert).
    let Some((cc, dir)) = check_env("bogus-size") else {
        return;
    };
    let bogus = "#include \"erl_nif.h\"\n\
                 _Static_assert(sizeof(ErlNifPid) == 999, \"the frozen ErlNifPid size is wrong\");\n";
    assert!(
        compile_probe(&cc, dir.path(), bogus).is_err(),
        "the structural ABI check must fail on a frozen size that does not hold"
    );
}
