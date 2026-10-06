#![allow(dead_code)]
//! Shared helpers for the `beam_abi` integration tests, plus the test-flavoured [`NifHost`].
//!
//! This module lives in a **subdirectory** of `tests/` on purpose: Cargo treats every
//! `tests/*.rs` file as its own integration-test binary, but a `tests/<dir>/mod.rs` is *not* a
//! target — it is a plain module that individual test files pull in with `mod common;`. That keeps
//! the helper code compiled and warning-free in each including binary while avoiding a spurious
//! "0 tests" target.
//!
//! The `#![allow(dead_code)]` above is required: the module is compiled separately into *every*
//! integration-test binary, so a helper unused by one binary would otherwise trip the `dead_code`
//! lint there (same convention as `beam_ets/tests/common/mod.rs`).
//!
//! # Skip-vs-fail policy (explicit)
//!
//! The NIF `.so` compilation helpers depend on host facilities that a bare container may lack: a C
//! compiler and the system `erl_nif.h`. When either is missing [`compile_so`] returns `None` and
//! prints a **loud** [`skip`] banner; the calling test must then return early. A skip is printed,
//! never silent.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, make_atom, Eterm};

/// The header the include-directory search keys on.
const HEADER: &str = "erl_nif.h";

// ---------------------------------------------------------------------------------------------
// Locating the system `erl_nif.h` include directory
// ---------------------------------------------------------------------------------------------

/// Locate the system `erl_nif.h` include directory, robustly; `None` if unavailable.
///
/// Resolution order (the first directory that actually contains a readable `erl_nif.h` wins):
///
/// 1. ask the system Erlang: `erl -noshell -eval 'io:format("~s",[code:lib_dir(erts,include)])'
///    -s init stop`. Some packaged runtimes (notably the Nix build) answer with a path that does
///    not exist, so the answer is accepted only if `<dir>/erl_nif.h` really is there;
/// 2. resolve the `erl` binary found on `PATH` and canonicalize it: an installed runtime ships the
///    real binary at `.../erts-<vsn>/bin/erl`, whose sibling `.../erts-<vsn>/include` holds
///    `erl_nif.h`;
/// 3. walk a small set of standard installation roots (`$ERL_TOP`, `/usr/lib/erlang`,
///    `/usr/local/lib/erlang`, `/usr/lib64/erlang`, `/opt/erlang`), checking `<root>/include` and
///    any `erts-*` directory directly under the root or under its `lib/erlang` subdirectory.
pub fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval()
        .or_else(from_path_erl)
        .or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}

/// True when `dir` holds a readable `erl_nif.h`.
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

/// Search one installation root for an include directory holding `erl_nif.h`.
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
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("erts-")
            {
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
// Compiling a NIF shared object
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

/// Compile C `source` (file stem `stem`) into a shared object in a fresh scratch dir.
///
/// Returns `None` (the caller must then [`skip`] LOUDLY) when there is no C compiler, no locatable
/// `erl_nif.h`, or the compiler fails; the compiler's stderr is printed to aid debugging. Every
/// failure path returns `None` — this never panics.
///
/// The command is `cc -shared -fPIC -I<include> -o <out.so> <src.c>`, run in a unique scratch
/// directory under [`std::env::temp_dir`] (named from the stem, the process id and a global
/// counter, so parallel test binaries do not collide).
pub fn compile_so(stem: &str, source: &str) -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip(&format!("{stem}: no erl_nif.h include directory found"));
        return None;
    };
    if !cc_available() {
        skip(&format!("{stem}: no C compiler ({} / $CC) available", cc()));
        return None;
    }

    let dir = scratch_dir(stem);
    let src = dir.join(format!("{stem}.c"));
    let so = dir.join(format!("{stem}.so"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&src, source));
    if let Err(err) = write {
        skip(&format!("{stem}: cannot prepare scratch dir {dir:?}: {err}"));
        return None;
    }

    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(&so)
        .arg(&src)
        .output();
    let Ok(output) = output else {
        skip(&format!("{stem}: could not run the C compiler"));
        return None;
    };
    if !output.status.success() {
        skip(&format!(
            "{stem}: cc failed (exit {:?}):\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    Some(so)
}

/// A fresh scratch directory for one compilation (unique per stem + process + call).
fn scratch_dir(stem: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("beam_abi-{stem}-{}-{n}", std::process::id()))
}

/// Print a loud skip banner (never a silent pass), test-name + reason.
pub fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

// ---------------------------------------------------------------------------------------------
// A minimal `NifHost` for tests
// ---------------------------------------------------------------------------------------------

/// A minimal [`beam_abi::NifHost`] for tests: a [`beam_alloc::Heap`] + an [`AtomTable`].
///
/// The declaration reads as a bare `pub struct TestHost`; it carries private state because
/// `NifHost::heap` must hand out `&mut beam_alloc::Heap` — the host has to *own* the arena and
/// the atom table.
pub struct TestHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
}

impl TestHost {
    /// A host with a comfortably sized heap and an empty atom table.
    pub fn new() -> Self {
        TestHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
        }
    }

    /// Intern `name` and return the atom term.
    pub fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    /// Mutable access to the atom table (for reverse lookups in assertions).
    pub fn atoms(&self) -> &AtomTable {
        &self.atoms
    }
}

impl Default for TestHost {
    fn default() -> Self {
        TestHost::new()
    }
}

impl beam_abi::NifHost for TestHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    /// The atom table is unbounded, so interning always succeeds.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    /// Reverse lookup: `None` for a non-atom term or an index not in the table.
    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }
}
