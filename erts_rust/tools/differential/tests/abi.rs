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

/// The `enif_*` symbols the structural check references (a representative slice of the
/// NIF ABI: allocation, term construction/access, resource types, messaging, I/O,
/// threads and synchronisation).
const NIF_SYMBOLS: &[&str] = &[
    "enif_alloc",
    "enif_free",
    "enif_make_atom",
    "enif_make_int",
    "enif_get_int",
    "enif_is_atom",
    "enif_make_tuple",
    "enif_get_tuple",
    "enif_priv_data",
    "enif_open_resource_type",
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
/// allocation, output, select, atom/term marshalling, async jobs, threads/synchronisation).
const DRIVER_SYMBOLS: &[&str] = &[
    "driver_alloc",
    "driver_free",
    "driver_output",
    "driver_select",
    "driver_mk_atom",
    "driver_exit",
    "driver_async",
    "erl_drv_thread_create",
    "erl_drv_mutex_create",
    "erl_drv_rwlock_create",
    "erl_drv_cond_create",
    "erl_drv_tsd_key_create",
    "erl_drv_send_term",
    "erl_drv_output_term",
];

/// The frozen types the two ABIs expose (`ErlNifEnv` is opaque by design, so it is only
/// referenced through a pointer).
const ABI_TYPES: &[&str] = &[
    "ErlNifPid",
    "ErlNifBinary",
    "ErlNifFunc",
    "ErlNifEnv",
    "ErlDrvBinary",
    "ErlDrvEntry",
    "SysIOVec",
];

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> std::io::Result<TempDir> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
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
        let value = match key {
            "CHAR" => sizes.get("CHAR").copied().unwrap_or(1),
            _ => match sizes.get(key) {
                Some(value) => *value,
                None => {
                    eprintln!("SKIP: the compiler does not define __SIZEOF_{key}__");
                    return None;
                }
            },
        };
        header.push_str(&format!("#define SIZEOF_{defined} {value}\n"));
    }
    Some(header)
}

/// The C translation unit: it only **references** the ABI declarations (address-of, so no
/// call is emitted) plus a few structural `sizeof` assertions.
fn probe_source() -> String {
    let mut src = String::from(
        "/* Structural A.3/A.4 check: reference the frozen NIF/driver C ABI. */\n\
         #include <stddef.h>\n\
         #include \"erl_nif.h\"\n\n",
    );
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

#[test]
fn the_frozen_nif_and_driver_declarations_still_compile() {
    let Some(cc) = resolve_cc() else {
        eprintln!("SKIP: no C compiler ($CC / cc / gcc / clang) on PATH — cannot run the A.3/A.4 structural check");
        return;
    };
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
    let Some(int_sizes) = int_sizes_for(&cc) else {
        eprintln!("SKIP: the target's integer sizes are not discoverable");
        return;
    };
    let dir = match TempDir::new("ok") {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("SKIP: cannot create a scratch directory: {err}");
            return;
        }
    };
    if let Err(err) = std::fs::write(dir.path().join("erl_int_sizes_config.h"), int_sizes) {
        eprintln!("SKIP: cannot write the synthesized config header: {err}");
        return;
    }

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
    let Some(cc) = resolve_cc() else {
        eprintln!("SKIP: no C compiler on PATH — cannot run the A.3/A.4 self-test");
        return;
    };
    let Some(int_sizes) = int_sizes_for(&cc) else {
        eprintln!("SKIP: the target's integer sizes are not discoverable");
        return;
    };
    let dir = match TempDir::new("bogus") {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("SKIP: cannot create a scratch directory: {err}");
            return;
        }
    };
    if std::fs::write(dir.path().join("erl_int_sizes_config.h"), int_sizes).is_err() {
        eprintln!("SKIP: cannot write the synthesized config header");
        return;
    }
    let bogus = "#include \"erl_nif.h\"\n\
                 static __typeof__(&enif_this_symbol_does_not_exist) bogus = \n\
                 &enif_this_symbol_does_not_exist;\n";
    assert!(
        compile_probe(&cc, dir.path(), bogus).is_err(),
        "the structural ABI check must fail on a symbol that does not exist"
    );
}
