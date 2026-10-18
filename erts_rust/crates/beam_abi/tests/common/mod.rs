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
//! Shared helpers for the `beam_abi` integration tests, plus the test-flavoured [`NifHost`] and
//! [`TestDriverHost`].
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
//! The shared-object compilation helpers depend on host facilities that a bare container may lack:
//! a C compiler and the system OTP include directory (which holds `erl_nif.h` *and*
//! `erl_driver.h`). When either is missing [`compile_so`] returns `None` and prints a **loud**
//! [`skip`] banner; the calling test must then return early. A skip is printed, never silent.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_abi::DriverHost;
use beam_abi::driver::types::{ErlDrvEvent, ErlDrvPort};
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, make_atom, Eterm};
use libc::{c_int, c_long, c_uint, c_ulong, c_void};

/// The header the include-directory search keys on. `erl_driver.h` (used by the driver fixtures)
/// ships in the **same** directory, so one probe resolves the include directory for both ABIs.
const HEADER: &str = "erl_nif.h";

// ---------------------------------------------------------------------------------------------
// Locating the system OTP include directory (`erl_nif.h` + `erl_driver.h`)
// ---------------------------------------------------------------------------------------------

/// Locate the system OTP include directory, robustly; `None` if unavailable.
///
/// The directory holds both `erl_nif.h` (keyed on below) and `erl_driver.h`, so the NIF and driver
/// fixtures share this one resolution.
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
// Compiling a NIF / driver shared object
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
/// OTP include directory (`erl_nif.h`/`erl_driver.h`), or the compiler fails; the compiler's stderr
/// is printed to aid debugging. Every failure path returns `None` — this never panics.
///
/// The command is `cc -shared -fPIC -I<include> -o <out.so> <src.c>`, run in a unique scratch
/// directory under [`std::env::temp_dir`] (named from the stem, the process id and a global
/// counter, so parallel test binaries do not collide).
pub fn compile_so(stem: &str, source: &str) -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip(&format!("{stem}: no OTP include directory (erl_nif.h/erl_driver.h) found"));
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

    /// Expose the owned table so an env-less comparison/hash orders an atom *nested* inside a
    /// compound by its name (the committed-table analogue of the runtime's `atom_table` hook).
    fn atom_table(&self) -> Option<&AtomTable> {
        Some(&self.atoms)
    }
}

// ---------------------------------------------------------------------------------------------
// A recording `DriverHost` for tests
// ---------------------------------------------------------------------------------------------

/// A [`beam_abi::DriverHost`] for tests that records every service call a driver makes.
///
/// Like [`TestHost`] it owns the arena the seam hands out (a [`beam_alloc::Heap`] and an
/// [`AtomTable`] for the term-data path); it additionally records the output/term traffic and the
/// select/timer/async calls, so a test can assert **exactly** what a real driver `.so` drove through
/// the exported `driver_*` functions.
pub struct TestDriverHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
    /// Every [`DriverHost::output`] byte string, in call order.
    outputs: Vec<Vec<u8>>,
    /// Every [`DriverHost::send_term`] call: `(destination, term)`.
    sent: Vec<(Option<Eterm>, Eterm)>,
    /// Every [`DriverHost::select`] call: `(mode, on)`.
    selects: Vec<(c_int, c_int)>,
    /// Every [`DriverHost::set_timer`] duration, in call order.
    timers: Vec<c_ulong>,
    /// The number of [`DriverHost::cancel_timer`] calls.
    cancels: usize,
    /// The number of [`DriverHost::r#async`] calls.
    asyncs: usize,
    /// The value [`DriverHost::read_timer`] reports (`None` = no timer pending).
    timer_left: Option<c_ulong>,
    /// The value [`DriverHost::connected_pid`] reports.
    connected: Option<Eterm>,
    /// The value [`DriverHost::caller_pid`] reports.
    caller: Option<Eterm>,
    /// The port id term [`DriverHost::port_term`] reports for a live handle (`None` = "no such
    /// handle" — the trait default's answer).
    port_term: Option<Eterm>,
    /// Every handle [`DriverHost::port_term`] was asked about, in call order.
    port_handles: Vec<ErlDrvPort>,
}
impl TestDriverHost {
    /// A host with a comfortably sized heap and an empty atom table.
    pub fn new() -> Self {
        TestDriverHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            outputs: Vec::new(),
            sent: Vec::new(),
            selects: Vec::new(),
            timers: Vec::new(),
            cancels: 0,
            asyncs: 0,
            timer_left: None,
            connected: None,
            caller: None,
            port_term: None,
            port_handles: Vec::new(),
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

    /// Every recorded [`DriverHost::output`] payload, in call order.
    pub fn outputs(&self) -> &[Vec<u8>] {
        &self.outputs
    }

    /// Every recorded [`DriverHost::send_term`] call: `(destination, term)`.
    pub fn sent(&self) -> &[(Option<Eterm>, Eterm)] {
        &self.sent
    }

    /// Every recorded [`DriverHost::select`] call: `(mode, on)`.
    pub fn selects(&self) -> &[(c_int, c_int)] {
        &self.selects
    }

    /// Every recorded [`DriverHost::set_timer`] duration, in call order.
    pub fn timers(&self) -> &[c_ulong] {
        &self.timers
    }

    /// The number of [`DriverHost::cancel_timer`] calls.
    pub fn cancels(&self) -> usize {
        self.cancels
    }

    /// The number of [`DriverHost::r#async`] calls.
    pub fn asyncs(&self) -> usize {
        self.asyncs
    }

    /// Set the value [`DriverHost::read_timer`] reports.
    pub fn set_timer_left(&mut self, ms: Option<c_ulong>) {
        self.timer_left = ms;
    }

    /// Set the value [`DriverHost::connected_pid`] returns.
    pub fn set_connected(&mut self, pid: Option<Eterm>) {
        self.connected = pid;
    }

    /// Set the value [`DriverHost::caller_pid`] returns.
    pub fn set_caller(&mut self, pid: Option<Eterm>) {
        self.caller = pid;
    }

    /// Set the port id term [`DriverHost::port_term`] reports for a live handle.
    pub fn set_port_term(&mut self, term: Option<Eterm>) {
        self.port_term = term;
    }

    /// Every handle [`DriverHost::port_term`] was asked about, in call order.
    pub fn port_handles(&self) -> &[ErlDrvPort] {
        &self.port_handles
    }
}

impl DriverHost for TestDriverHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    /// The atom table is unbounded, so interning always succeeds.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.outputs.push(data.to_vec());
        0
    }

    fn send_term(&mut self, _port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int {
        self.sent.push((to, term));
        0
    }

    fn select(&mut self, _port: ErlDrvPort, _event: ErlDrvEvent, mode: c_int, on: c_int) -> c_int {
        self.selects.push((mode, on));
        0
    }

    fn set_timer(&mut self, _port: ErlDrvPort, ms: c_ulong) -> c_int {
        self.timers.push(ms);
        0
    }

    fn cancel_timer(&mut self, _port: ErlDrvPort) -> c_int {
        self.cancels += 1;
        0
    }

    fn read_timer(&mut self, _port: ErlDrvPort) -> Option<c_ulong> {
        self.timer_left
    }

    fn r#async(
        &mut self,
        _port: ErlDrvPort,
        _key: *mut c_uint,
        _invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        _data: *mut c_void,
        _free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        self.asyncs += 1;
        0
    }

    fn connected_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        self.connected
    }

    fn caller_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        self.caller
    }

    /// Answer the configured port id term for a **live** handle only.
    ///
    /// A live handle is the [`beam_abi::DriverEnv`] pointer handed to the driver, i.e. non-null and
    /// 8-aligned, while a real internal port term always has its low bits set — the same
    /// disambiguation the decoder relies on. Any other word is "no such handle" (`None`), which is
    /// what keeps a malformed `ERL_DRV_PORT` operand refused.
    fn port_term(&mut self, port: ErlDrvPort) -> Option<Eterm> {
        self.port_handles.push(port);
        let aligned = (port as usize) & (core::mem::align_of::<usize>() - 1) == 0;
        if !port.is_null() && aligned {
            self.port_term
        } else {
            None
        }
    }
}