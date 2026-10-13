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

//! The real `erl_ddll` loading BIFs — `try_load/3`, `try_unload/2`, `loaded_drivers/0`.
//!
//! These three keys are what `erlang:load_driver/2` and `erl_ddll` drive to `dlopen`, list and
//! drop a dynamically linked driver. They are plain [`BifFn`](crate::registry::BifFn)s (not
//! `RuntimeHooks` seam keys), because the only runtime state they need is the loaded-driver
//! registry ([`super::registry`]) plus the safe loader `beam_abi::driver::loader::load` — no
//! `&mut Runtime`.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bif_ddll.c` —
//! `erl_ddll_try_load_3`/`erl_ddll_try_unload_2`/`erl_ddll_loaded_drivers_0`, `try_load`/`unload_driver`
//! (`:1830-1980`), `build_load_error_hp` (`:1770`) and `pick_list_or_atom` — with the platform half
//! in `erts/emulator/sys/unix/erl_unix_sys_ddll.c`.
//!
//! # The observable contract
//!
//! * `try_load(Path, Name, Options)` — `Path`/`Name` are a **charlist** or an **atom**
//!   (`pick_list_or_atom`); the shared object is `<Path>/<Name>.so`. A driver already in the
//!   registry answers `{ok, already_loaded}`; a fresh successful load answers `{ok, loaded}`. A load
//!   *failure* is the result `{error, Reason}` (never a raise) — see the error mapping below.
//! * `try_unload(Name, Options)` — a name that is not loaded answers `{error, not_loaded}`; a loaded
//!   one is removed (dropping the [`DriverLibrary`](beam_abi::driver::loader::DriverLibrary) runs
//!   the driver's `finish` callback and `dlclose`s the object) and answers `ok`.
//! * `loaded_drivers()` — `{ok, [Name, …]}` with every name a **charlist**.
//!
//! # The error mapping (`build_load_error_hp`, `erl_bif_ddll.c:1770`)
//!
//! A [`DriverLoadError`] whose reason is one of the fixed `errcode_tab` codes `0..=-8` becomes the
//! **bare reason atom**; anything past `DYNAMIC_ERROR_OFFSET` (`-10`) becomes the 2-tuple
//! `{open_error, Code}`. The loader reports a failed `dlopen` as
//! [`DriverLoadError::Unspecified`], which this module maps to `{open_error, -10}` — the C's own
//! `-10 - <index into the platform's dlerror table>`, approximated here by its first/common case
//! `-10`. Every other variant maps to its bare [`DriverLoadError::reason_atom`].
//!
//! # Honest divergences
//!
//! * **The option set is scoped.** Only the `kill_ports` flag and the `{monitor, _}` tuple are
//!   recognised (the latter's payload is not validated); the C additionally handles `{reload, …}` /
//!   `{driver_options, …}` and the whole pending-reload state machine. Any other element is `badarg`.
//! * **Unload is immediate** (not the C's pending state machine), and `monitor`/`demonitor`/`info`
//!   remain typed `unsupported_bif` seam keys in [`crate::runtime::other`].
//! * **No statically linked built-ins**: the oracle lists `tcp_inet`/`udp_inet`/`ram_file_drv`;
//!   this runtime links none, so a fresh registry is empty.

use beam_abi::driver::loader::{load, DriverLoadError};
use beam_common::Result;
use beam_term::prelude::signed_value;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::badarg;
use crate::print::proper_list;
use crate::registry::BifRegistry;

use super::registry::{self, Loaded};

/// The `(module, function, arity)` keys this module registers **for real**.
///
/// Sorted by `(module, function, arity)`. The three remaining `erl_ddll` keys
/// (`demonitor/1`, `info/2`, `monitor/2`) stay seam-backed in [`crate::runtime::other`].
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erl_ddll", "loaded_drivers", 0),
    ("erl_ddll", "try_load", 3),
    ("erl_ddll", "try_unload", 2),
];

/// `ERL_DE_DYNAMIC_ERROR_OFFSET` (`global.h:181`) — the first dynamic open-error code, and the code
/// this runtime reports for a failed `dlopen` (see the module docs).
const OPEN_ERROR_CODE: i64 = -10;

/// `erl_ddll:try_load/3` — `dlopen` a driver named by a path/name pair.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Path`/`Name` is neither a charlist nor
/// an atom, or `Options` is not a proper list of the recognised options. Propagates a heap-allocation
/// failure from building the result tuple. A load failure is a normal result, never an error.
fn try_load_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let path = pick_list_or_atom(ctx, args[0])?;
    let name = pick_list_or_atom(ctx, args[1])?;
    let (options, kill_ports) = parse_options(ctx, args[2])?;
    if registry::contains(&name) {
        return ok_atom_tuple(ctx, "already_loaded");
    }
    match load(&format!("{path}/{name}"), &name) {
        Ok(library) => {
            registry::insert(
                name,
                Loaded {
                    path,
                    library,
                    options,
                    kill_ports,
                    loader: ctx.self_pid(),
                },
            );
            ok_atom_tuple(ctx, "loaded")
        }
        Err(error) => load_error_tuple(ctx, &error),
    }
}

/// `erl_ddll:try_unload/2` — drop a loaded driver.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a malformed `Name`/`Options`. An
/// unknown name is the result `{error, not_loaded}`.
fn try_unload_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let name = pick_list_or_atom(ctx, args[0])?;
    let _options = parse_options(ctx, args[1])?;
    if registry::remove(&name).is_none() {
        return error_atom_tuple(ctx, "not_loaded");
    }
    ctx.intern_atom("ok")
}

/// `erl_ddll:loaded_drivers/0` — every loaded driver's name as a charlist.
///
/// # Errors
///
/// Propagates a heap-allocation failure from building the result list.
fn loaded_drivers_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let names = registry::names();
    let mut lists = Vec::with_capacity(names.len());
    for name in &names {
        lists.push(super::charlist(ctx, name)?);
    }
    let list = ctx.list(&lists)?;
    ok_tuple(ctx, list)
}

/// The C `pick_list_or_atom`: a charlist of bytes, or an atom name.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for any other term, a non-byte element, or a
/// charlist whose bytes are not valid UTF-8 (this runtime's loader takes a `&str`, so a raw
/// non-UTF-8 path cannot be conveyed — the C passes bytes straight to `dlopen`).
fn pick_list_or_atom(ctx: &BifContext<'_>, term: Eterm) -> Result<String> {
    if access::is_atom(term) {
        return ctx
            .atoms()
            .name(atom_val(term))
            .map(str::to_owned)
            .ok_or_else(badarg);
    }
    let elems = proper_list(term).ok_or_else(badarg)?;
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in elems {
        if !access::is_small(elem) {
            return Err(badarg());
        }
        let byte = u8::try_from(signed_value(elem)).map_err(|_| badarg())?;
        bytes.push(byte);
    }
    String::from_utf8(bytes).map_err(|_| badarg())
}

/// The C's `try_load` option list: the `kill_ports` flag and the `{monitor, _}` tuple.
///
/// Returns the recognised option names plus whether `kill_ports` was given.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list, an improper list, or any
/// element that is neither `kill_ports` nor a 2-tuple tagged `monitor`.
fn parse_options(ctx: &BifContext<'_>, term: Eterm) -> Result<(Vec<String>, bool)> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    let mut options = Vec::new();
    let mut kill_ports = false;
    for elem in elems {
        if named_atom(ctx, elem, "kill_ports") {
            kill_ports = true;
            options.push("kill_ports".to_string());
        } else if is_monitor_option(ctx, elem) {
            options.push("monitor".to_string());
        } else {
            return Err(badarg());
        }
    }
    Ok((options, kill_ports))
}

/// Whether `term` is the atom `name`.
fn named_atom(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// Whether `term` is the 2-tuple `{monitor, _}`.
fn is_monitor_option(ctx: &BifContext<'_>, term: Eterm) -> bool {
    access::is_tuple(term)
        && access::tuple_size(term) == 2
        && named_atom(ctx, access::tuple_element(term, 0), "monitor")
}

/// `{ok, Value}`.
fn ok_tuple(ctx: &mut BifContext<'_>, value: Eterm) -> Result<Eterm> {
    let ok = ctx.intern_atom("ok")?;
    ctx.tuple(&[ok, value])
}

/// `{ok, Atom}` for a bare atom tag.
fn ok_atom_tuple(ctx: &mut BifContext<'_>, tag: &str) -> Result<Eterm> {
    let tag = ctx.intern_atom(tag)?;
    ok_tuple(ctx, tag)
}

/// `{error, Atom}` for a bare atom reason.
fn error_atom_tuple(ctx: &mut BifContext<'_>, reason: &str) -> Result<Eterm> {
    let error = ctx.intern_atom("error")?;
    let reason = ctx.intern_atom(reason)?;
    ctx.tuple(&[error, reason])
}

/// `{error, Reason}` for a loader failure (`build_load_error_hp`, `erl_bif_ddll.c:1770`).
fn load_error_tuple(ctx: &mut BifContext<'_>, error: &DriverLoadError) -> Result<Eterm> {
    match error {
        DriverLoadError::Unspecified(_) => {
            let error = ctx.intern_atom("error")?;
            let open_error = ctx.intern_atom("open_error")?;
            let code = ctx.int(OPEN_ERROR_CODE)?;
            let reason = ctx.tuple(&[open_error, code])?;
            ctx.tuple(&[error, reason])
        }
        other => error_atom_tuple(ctx, other.reason_atom()),
    }
}

/// Registers the three real `erl_ddll` loading BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erl_ddll", "loaded_drivers", 0, loaded_drivers_0);
    reg.register("erl_ddll", "try_load", 3, try_load_3);
    reg.register("erl_ddll", "try_unload", 2, try_unload_2);
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_atom, make_small, NIL};

    /// The atoms the tests build or route by name.
    const SEEDED: [&str; 14] = [
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
    ];

    /// The three `erl_ddll` loading BIFs over their own heap + seeded atom table.
    struct Fixture {
        registry: BifRegistry,
        heap: Heap,
        atoms: beam_term::AtomTable,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut atoms = beam_term::AtomTable::new();
            atoms.seed(SEEDED);
            let mut registry = BifRegistry::new();
            super::register(&mut registry);
            Fixture {
                registry,
                heap: Heap::new(1 << 16).expect("test heap"),
                atoms,
            }
        }

        /// A seeded atom term.
        fn atom(&self, name: &str) -> Eterm {
            make_atom(self.atoms.lookup(name).expect("a seeded atom"))
        }

        /// A charlist of `text`'s bytes.
        fn charlist(&mut self, text: &str) -> Eterm {
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
        fn cons(&mut self, term: Eterm) -> Eterm {
            self.heap.cons(term, NIL).expect("a cons fits")
        }

        /// `[term | tail]`.
        fn cons_tail(&mut self, term: Eterm, tail: Eterm) -> Eterm {
            self.heap.cons(term, tail).expect("a cons fits")
        }        /// Calls one of the module's BIFs.
        fn call(&mut self, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            self.registry
                .call("erl_ddll", function, arity, &mut ctx, args)
        }

        /// Renders `term` as `io_lib:write/1` (non-printable, so a charlist is its byte list).
        fn render(&mut self, term: Eterm) -> String {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            crate::print::format_term(&mut ctx, term, false).expect("a printable term")
        }
    }

    /// The registry is a `thread_local`, so a test that runs after another on the same thread would
    /// otherwise inherit its drivers. Each test starts from an empty registry.
    fn clear_registry() {
        for name in crate::runtime::ddll::registry::names() {
            crate::runtime::ddll::registry::remove(&name);
        }
    }

    #[test]
    fn loaded_drivers_is_empty_on_a_fresh_registry() {
        clear_registry();
        let mut fx = Fixture::new();
        let out = fx
            .call("loaded_drivers", 0, &[])
            .expect("loaded_drivers/0 never fails");
        assert_eq!(fx.render(out), "{ok,[]}");
    }

    #[test]
    fn try_unload_of_an_unknown_driver_is_not_loaded() {
        clear_registry();
        let mut fx = Fixture::new();
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("an unload failure is a result, not an error");
        assert_eq!(fx.render(out), "{error,not_loaded}");
    }

    #[test]
    fn try_load_of_a_missing_object_is_open_error() {
        clear_registry();
        let mut fx = Fixture::new();
        let path = fx.charlist("/nonexistent/beam_bifs/ddll");
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a load failure is a result, not an error");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");
    }

    #[test]
    fn an_atom_names_a_driver_like_a_charlist() {
        clear_registry();
        let mut fx = Fixture::new();
        // An atom `Path`/`Name` is `pick_list_or_atom`'s other arm; a missing object still yields the
        // loader's open error, so the decode did not reject the atom.
        let path = fx.atom("no_driver_init");
        let name = fx.atom("no_driver_init");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a load failure is a result");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");
    }

    #[test]
    fn a_malformed_argument_is_badarg() {
        clear_registry();
        let mut fx = Fixture::new();
        let int = make_small(1);

        // A non-list, non-atom `Path`.
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[int, path, NIL]),
            Err(BeamError::BadArg)
        ));
        // `Options` that is not a list.
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[path, path, int]),
            Err(BeamError::BadArg)
        ));
        // An unrecognised option element.
        let options = fx.cons(int);
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[path, path, options]),
            Err(BeamError::BadArg)
        ));
        // A charlist with a non-byte element.
        let bad = fx.cons(make_small(999));
        let path = fx.charlist("x");
        assert!(matches!(
            fx.call("try_load", 3, &[bad, path, NIL]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn the_recognised_options_are_accepted_and_the_rest_are_not() {
        clear_registry();
        let mut fx = Fixture::new();
        // `[kill_ports, {monitor, self()}]` parses, so the call reaches the loader and reports the
        // missing object rather than `badarg`.
        let monitor = {
            let tag = fx.atom("monitor");
            let payload = make_small(1);
            fx.heap.tuple(&[tag, payload]).expect("a tuple fits")
        };
        let kill_ports = fx.atom("kill_ports");
        let mut options = NIL;
        options = fx.cons_tail(monitor, options);
        options = fx.cons_tail(kill_ports, options);
        let path = fx.charlist("/nonexistent/beam_bifs/ddll");
        let name = fx.charlist("no_such_driver");
        let out = fx
            .call("try_load", 3, &[path, name, options])
            .expect("a load failure is a result");
        assert_eq!(fx.render(out), "{error,{open_error,-10}}");
    }

    // -----------------------------------------------------------------------------------------
    // The real shared-object fixture (compiled with the system `cc`)
    // -----------------------------------------------------------------------------------------

    /// A minimal driver: `driver_init` returns an `ErlDrvEntry` named `test_drv` with the three
    /// callbacks the objective requires and no `driver_*` import (so `dlopen(RTLD_NOW)` resolves).
    const DRIVER_FIXTURE_C: &str = r#"
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
    fn skip(reason: &str) {
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
    fn erl_include_dir() -> Option<PathBuf> {
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
    fn which_erl() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("erl"))
            .find(|candidate| candidate.is_file())
    }

    /// Compile [`DRIVER_FIXTURE_C`] into `<scratch>/test_drv.so` and return `<scratch>`.
    ///
    /// Returns `None` (after a loud [`skip`]) when the toolchain is absent; [`try_load`] appends the
    /// `.so`, so the caller passes the directory alone.
    fn compile_driver_fixture() -> Option<String> {
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

    #[test]
    fn a_real_driver_loads_lists_and_unloads() {
        clear_registry();
        let Some(dir) = compile_driver_fixture() else {
            return;
        };
        let mut fx = Fixture::new();

        // --- fresh load -----------------------------------------------------------------------
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a first load is a result");
        assert_eq!(fx.render(out), "{ok,loaded}");

        // --- the registry now holds it ---------------------------------------------------------
        assert!(crate::runtime::ddll::registry::contains("test_drv"));
        let out = fx
            .call("loaded_drivers", 0, &[])
            .expect("loaded_drivers/0 never fails");
        assert_eq!(fx.render(out), "{ok,[[116,101,115,116,95,100,114,118]]}");

        // --- a second load of the same name ----------------------------------------------------
        let path = fx.charlist(&dir);
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_load", 3, &[path, name, NIL])
            .expect("a repeat load is a result");
        assert_eq!(fx.render(out), "{ok,already_loaded}");

        // --- unload ---------------------------------------------------------------------------
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("an unload is a result");
        assert_eq!(fx.render(out), "ok");
        assert!(!crate::runtime::ddll::registry::contains("test_drv"));
        let out = fx
            .call("loaded_drivers", 0, &[])
            .expect("loaded_drivers/0 never fails");
        assert_eq!(fx.render(out), "{ok,[]}");

        // --- unloading again is not_loaded -----------------------------------------------------
        let name = fx.charlist("test_drv");
        let out = fx
            .call("try_unload", 2, &[name, NIL])
            .expect("a repeat unload is a result");
        assert_eq!(fx.render(out), "{error,not_loaded}");
    }

    #[test]
    fn the_roster_is_registered() {
        assert_eq!(
            REAL_BIFS,
            &[
                ("erl_ddll", "loaded_drivers", 0),
                ("erl_ddll", "try_load", 3),
                ("erl_ddll", "try_unload", 2),
            ]
        );
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert_eq!(reg.len(), 3, "the module registers exactly its roster");
        for (module, function, arity) in REAL_BIFS {
            assert!(reg.contains(module, function, *arity));
        }
    }
}
