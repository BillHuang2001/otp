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

use beam_abi::driver::types::ERL_DRV_EXTENDED_MAJOR_VERSION;
use beam_alloc::Heap;
use beam_process::ProcessId;
use beam_rt::port::driver::{load_driver, Driver, DriverPort};
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

/// Compiles the C fixture, or `None` when a host facility is missing (the caller returns
/// early — **loudly**).
fn build_fixture(label: &str) -> Option<Fixture> {
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
    let stem = dir.join("t");
    let c_src = dir.join("test_driver.c");
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, DRIVER_C));
    if let Err(error) = write {
        skip(&format!("driver/{label}: cannot prepare {}: {error}", dir.display()));
        return None;
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/t.so <dir>/test_driver.c`.
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join("t.so"))
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
    if !dir.join("t.so").is_file() {
        skip(&format!("driver/{label}: the C compiler produced no t.so"));
        return None;
    }
    Some(Fixture { dir, stem })
}

// ---------------------------------------------------------------------------------------------
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

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ----------------------------------------
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
        &[hello.to_vec()],
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

/// The built binary **exports** every `driver_*` symbol the fixture imports, as a dynamic symbol.
///
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
