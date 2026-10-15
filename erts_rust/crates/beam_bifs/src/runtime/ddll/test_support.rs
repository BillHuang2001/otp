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

//! Shared test scaffolding for the `erl_ddll` submodules' unit tests.
//!
//! The three `erl_ddll` submodules (`live`, `info`, `monitor`) drive the same crate
//! surface — a [`BifRegistry`] holding the whole `erl_ddll` roster, a real
//! [`Heap`] + a seeded [`AtomTable`], and (for the loading keys) a real
//! `dlopen`-able driver shared object compiled with the system `cc`. That machinery
//! lives here once (the three test modules import it) rather than triplicated.
//!
//! Everything is `#[cfg(test)]`-gated at its sole `mod` declaration in `ddll.rs`.

use std::path::PathBuf;

use beam_alloc::Heap;
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, AtomTable, Eterm, NIL};

use crate::context::BifContext;
use crate::registry::BifRegistry;

/// The atoms the tests build or route by name.
pub(crate) const SEEDED: [&str; 30] = [
    // The loading keys' atoms.
    "ok",
    "error",
    "loaded",
    "already_loaded",
    "open_error",
    "not_loaded",
    "kill_ports",
    "monitor",
    "no_driver_init",
    "driver_init_failed",
    "bad_driver_name",
    "driver_incorrect_version",
    "symbol_not_found",
    "unspecified_error",
    // `info/2`'s item atoms and the `monitor/2` argument/item atoms.
    "driver",
    "unloaded",
    "unloaded_only",
    "linked_in_driver",
    "permanent",
    "processes",
    "port_count",
    "driver_options",
    "awaiting_load",
    "awaiting_unload",
    "true",
    "false",
    // The driver fixture's name (so `info/2`'s atom-name arm can name a loaded driver).
    "test_drv",
    // The statically linked built-ins (so `try_load`/`try_unload`/`info/2` can name them by atom).
    "tcp_inet",
    "udp_inet",
    "ram_file_drv",
];

/// An `erl_ddll` call fixture over its own heap, seeded atom table and a registry
/// holding the whole `erl_ddll` roster.
pub(crate) struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    /// Builds the fixture, registering every real `erl_ddll` key.
    pub(crate) fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(SEEDED);
        let mut registry = BifRegistry::new();
        register_all(&mut registry);
        Fixture {
            registry,
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms,
        }
    }

    /// A seeded atom term.
    pub(crate) fn atom(&self, name: &str) -> Eterm {
        make_atom(self.atoms.lookup(name).expect("a seeded atom"))
    }

    /// A charlist of `text`'s bytes.
    pub(crate) fn charlist(&mut self, text: &str) -> Eterm {
        let mut tail = NIL;
        for byte in text.bytes().rev() {
            tail = self
                .heap
                .cons(make_small(i64::from(byte)), tail)
                .expect("the charlist fits");
        }
        tail
    }

    /// `[term]` — a cons cell over a fresh tail.
    pub(crate) fn cons(&mut self, term: Eterm) -> Eterm {
        self.heap.cons(term, NIL).expect("a cons fits")
    }

    /// `[term | tail]`.
    pub(crate) fn cons_tail(&mut self, term: Eterm, tail: Eterm) -> Eterm {
        self.heap.cons(term, tail).expect("a cons fits")
    }

    /// A `size`-tuple of `elems` on the fixture heap.
    pub(crate) fn tuple(&mut self, elems: &[Eterm]) -> Eterm {
        self.heap.tuple(elems).expect("a tuple fits")
    }

    /// Calls one of the module's `erl_ddll` BIFs with the fixture's `self()` = `ProcessId::new(1, 0)`.
    pub(crate) fn call(&mut self, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call("erl_ddll", function, arity, &mut ctx, args)
    }

    /// Renders `term` as `io_lib:write/1` (non-printable, so a charlist is its byte list).
    pub(crate) fn render(&mut self, term: Eterm) -> String {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        crate::print::format_term(&mut ctx, term, false).expect("a printable term")
    }
}

/// Registers every real `erl_ddll` key (the loading keys, `info/2` and the monitor pair).
fn register_all(reg: &mut BifRegistry) {
    crate::runtime::ddll::live::register(reg);
    crate::runtime::ddll::info::register(reg);
    crate::runtime::ddll::monitor::register(reg);
}

/// The registry is a `thread_local`, so a test that runs after another on the same thread would
/// otherwise inherit its drivers. Each test starts from an empty registry.
pub(crate) fn clear_registry() {
    for name in crate::runtime::ddll::registry::names() {
        crate::runtime::ddll::registry::remove(&name);
    }
}

// ---------------------------------------------------------------------------------------------
// The real shared-object fixture (compiled with the system `cc`)
// ---------------------------------------------------------------------------------------------

/// A minimal driver: `driver_init` returns an `ErlDrvEntry` named `test_drv` with the three
/// callbacks the objective requires and no `driver_*` import (so `dlopen(RTLD_NOW)` resolves).
pub(crate) const DRIVER_FIXTURE_C: &str = r#"
#include <erl_driver.h>

static ErlDrvData test_start(ErlDrvPort port, char *command) {
    (void) port;
    (void) command;
    return (ErlDrvData) 0;
}

static void test_stop(ErlDrvData data) { (void) data; }

static void test_output(ErlDrvData data, char *buf, ErlDrvSizeT len) {
    (void) data;
    (void) buf;
    (void) len;
}

static ErlDrvEntry test_drv_entry = {
    .driver_name = "test_drv",
    .start = test_start,
    .stop = test_stop,
    .output = test_output,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(test_drv) { return &test_drv_entry; }
"#;

/// Print a loud skip banner (never a silent pass).
pub(crate) fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

/// The installed OTP include directory holding `erl_driver.h`, if any.
///
/// Resolution order (the first directory that really holds `erl_driver.h` wins): the answer of
/// `code:lib_dir(erts, include)` — accepted only when the header is actually there, since some
/// packaged runtimes (notably the Nix build) answer with a non-existent path — then the sibling
/// `include` of the `erts-*` directory of the `erl` on `PATH`.
pub(crate) fn erl_include_dir() -> Option<PathBuf> {
    fn has_header(dir: &std::path::Path) -> bool {
        dir.join("erl_driver.h").is_file()
    }

    if let Ok(output) = std::process::Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir(erts,include)])",
            "-s",
            "init",
            "stop",
        ])
        .output()
    {
        if output.status.success() {
            if let Ok(text) = String::from_utf8(output.stdout) {
                let dir = PathBuf::from(text.trim());
                if has_header(&dir) {
                    return Some(dir);
                }
            }
        }
    }

    let erl = which_erl()?;
    // `.../erts-<vsn>/bin/erl` -> `.../erts-<vsn>/include`
    let erts = erl.canonicalize().ok()?.parent()?.parent().map(PathBuf::from)?;
    let include = erts.join("include");
    has_header(&include).then_some(include)
}

/// The first `erl` executable on `PATH`.
pub(crate) fn which_erl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("erl"))
        .find(|candidate| candidate.is_file())
}

/// Compile [`DRIVER_FIXTURE_C`] into `<scratch>/test_drv.so` and return `<scratch>`.
///
/// Returns `None` (after a loud [`skip`]) when the toolchain is absent; `try_load` appends the
/// `.so`, so the caller passes the directory alone.
pub(crate) fn compile_driver_fixture() -> Option<String> {
    let Some(include) = erl_include_dir() else {
        skip("ddll driver fixture: no OTP include directory (erl_driver.h) found");
        return None;
    };
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    if !std::process::Command::new(&cc)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
    {
        skip(&format!("ddll driver fixture: no C compiler ({cc} / $CC) available"));
        return None;
    }

    let dir = std::env::temp_dir().join(format!(
        "beam_bifs-ddll-driver-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let src = dir.join("test_drv.c");
    let so = dir.join("test_drv.so");
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&src, DRIVER_FIXTURE_C));
    if let Err(err) = write {
        skip(&format!("ddll driver fixture: cannot prepare {dir:?}: {err}"));
        return None;
    }

    let output = std::process::Command::new(&cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(&so)
        .arg(&src)
        .output();
    let Ok(output) = output else {
        skip("ddll driver fixture: could not run the C compiler");
        return None;
    };
    if !output.status.success() {
        skip(&format!(
            "ddll driver fixture: cc failed (exit {:?}):\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    Some(dir.to_string_lossy().into_owned())
}
