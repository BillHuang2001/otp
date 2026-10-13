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

//! The A.4 **driver-registry-unification regression suite**: the runtime's one name-keyed
//! loaded-driver list ([`LOADED_DRIVERS`](super::LOADED_DRIVERS)) is *the* list `erl_ddll:*`
//! reports **and** the one `open_port({spawn_driver, Name}, …)` resolves a name against — whichever
//! way a driver was loaded.
//!
//! Two writers fill that list: [`record_driver_loaded`](super::record_driver_loaded) from an
//! `erl_ddll:try_load/3` (owning the `dlopen` handle) and
//! [`note_driver_opened`](super::note_driver_opened) from a port
//! `open_port({spawn_driver, …})` `dlopen`ed (the port owns the handle). The two readers are the
//! same list from either side:
//!
//! * **direction (i)** — a driver `erl_ddll:try_load/3` loaded must be openable by its **name**
//!   through `open_port({spawn_driver, Name}, …)` (the port re-uses the recorded object path), and
//!   must be **listed** by `erl_ddll:loaded_drivers/0`;
//! * **direction (ii)** — a driver reached only through a port
//!   `open_port({spawn_driver, <object path>}, …)` `dlopen`ed must likewise appear in
//!   `erl_ddll:loaded_drivers/0` (its *declared* `driver_name`, the object's file stem).
//!
//! Before the unification each view was its own list: `beam_bifs`'s `erl_ddll:*` registry knew only
//! what its own `try_load/3` had recorded, so a name a port had opened was invisible to
//! `erl_ddll:loaded_drivers/0`, and an `erl_ddll:try_load/3`ed name could not be opened by name.
//! This suite pins the single shared view end to end with a **real C driver** compiled against the
//! host `erl_driver.h`.
//!
//! # The observed answers (the oracle's own shapes)
//!
//! * `erl_ddll:loaded_drivers/0` ⇒ `{ok, [Name, …]}`, each name a charlist, in load order;
//! * `erl_ddll:try_load/3` ⇒ `{ok, loaded}` then `{ok, already_loaded}`;
//! * `erl_ddll:info/2` ⇒ the item value of a **listed** name, `badarg` for an unknown name or item;
//! * `erl_ddll:try_unload/2` ⇒ `ok`, then `{error, not_loaded}`.
//!
//! # Documented divergences (asserted, not closed)
//!
//! * a fresh `loaded_drivers/0` is `{ok, []}` here, where the oracle lists its statically linked
//!   `tcp_inet`/`udp_inet`/`ram_file_drv` (the runtime links no built-in drivers);
//! * `open_port({spawn_driver, Name}, …)` for an **unloaded** name auto-`dlopen`s here (a
//!   `/`-containing name is the object path, the driver name being its file stem), where the oracle
//!   answers `badarg` for a name it has not loaded — which is exactly what makes direction (ii)
//!   observable;
//! * `try_unload/2` is an immediate `ok`, where the oracle answers `{ok, pending_process}` /
//!   `{ok, pending_driver}` while ports still hold the driver.
//!
//! # Skip policy
//!
//! Needs the host C compiler and the host `erl_driver.h`; without either it prints a **loud**
//! `SKIP:` banner and returns — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_bifs::NullSink;
use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_process::PortId;
use beam_term::{signed_value, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::runtime::Runtime;

/// The fixture: a minimal driver that declares the name it was compiled under.
///
/// `start` records a non-negative private state and owns no descriptor, timer or child — this suite
/// is about which **list** the driver is recorded in, not about driving it.
const DDLL_C: &str = r#"
#include <erl_driver.h>

static ErlDrvData rt_ddll_start(ErlDrvPort port, char *command) {
    (void) port;
    (void) command;
    return (ErlDrvData) 1;  /* any non-negative private state */
}

static void rt_ddll_stop(ErlDrvData drv_data) {
    (void) drv_data;
}

static ErlDrvEntry rt_ddll_entry = {
    .init = NULL,
    .start = rt_ddll_start,
    .stop = rt_ddll_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_DDLL_NAME,
    .finish = NULL,
    .handle = NULL,
    .control = NULL,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = 0,
    .handle2 = NULL,
    .process_exit = NULL,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(rt_ddll_init) {
    return &rt_ddll_entry;
}
"#;

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

/// The host `erl_driver.h` include directory, or `None` when it cannot be found.
///
/// A compact duplicate of `control_tests.rs::erl_include_dir` (a unit test cannot import the
/// `tests/` crate target's support, and each A.4 fixture file carries its own copy of this probe —
/// see `control_tests.rs`'s own note on `stop_select_tests.rs`). Resolution order: ask the host
/// `erl`; then the `include` sibling of the `erts-*` directory the `erl` on `$PATH` lives in; then
/// the standard roots.
fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval()
        .or_else(from_path_erl)
        .or_else(|| {
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
            roots.into_iter().find_map(|root| search_root(&root))
        })
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
    let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    has_header(&dir).then_some(dir)
}

/// The `include` sibling of the `erts-*` directory the `erl` on `$PATH` lives in.
fn from_path_erl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let erl = std::env::split_paths(&path)
        .map(|dir| dir.join("erl"))
        .find(|candidate| candidate.is_file())?
        .canonicalize()
        .ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    has_header(&include).then_some(include)
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

/// A compiled driver `.so` plus the name its entry declares.
struct Fixture {
    /// The scratch directory holding the source and the `.so`.
    dir: PathBuf,
    /// The object path **with** the `.so` suffix (the `driver_lookup` path form).
    object: PathBuf,
    /// The `driver_name` the library is compiled under — the object's file stem, and the name the
    /// loader validates the entry against.
    name: String,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The scratch directory as the `erl_ddll:try_load/3` `Path` argument.
    fn dir_path(&self) -> String {
        self.dir.display().to_string()
    }

    /// The object path as the `open_port({spawn_driver, …})` `Name`.
    fn object_path(&self) -> String {
        self.object.display().to_string()
    }
}

/// Compiles [`DDLL_C`] as `<scratch>/<stem>.so` with `RT_DDLL_NAME="<stem>"`; `None` when a host
/// facility is missing (the caller returns early — **loudly**).
fn build_fixture(stem: &str) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!("ddll: no C compiler ({} / $CC) available", cc()));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("ddll: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_ddll_{stem}_{}_{n}", std::process::id()));
    let source = dir.join(format!("{stem}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, DDLL_C));
    if let Err(error) = write {
        skip(&format!("ddll: cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let object = dir.join(format!("{stem}.so"));
    // `cc -shared -fPIC -DRT_DDLL_NAME="<stem>" -I<include> -o <so> <c>`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_DDLL_NAME=\"{stem}\""))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(&object)
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "ddll: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("ddll: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !object.is_file() {
        skip(&format!("ddll: the C compiler produced no {stem}.so"));
        return None;
    }
    Some(Fixture {
        dir,
        object,
        name: stem.to_string(),
    })
}

// ---------------------------------------------------------------------------------------------
// Driving the runtime
// ---------------------------------------------------------------------------------------------

/// A runtime over a code-path-less code server plus the core atom seed.
fn runtime() -> Runtime {
    // The same anchor `crate::port::PortRegistry::new` uses, so a fixture driver's `dlopen`ed
    // `driver_*` imports bind at `RTLD_NOW` out of the loading process's dynamic symbol table (see
    // the crate's `build.rs`). Cheap and harmless to repeat here.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    Runtime::with_defaults().expect("the default runtime")
}

/// Runs `module:function(args)` through the runtime's own dispatch (`call_bif`).
fn call(runtime: &mut Runtime, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    context.call_bif(module, function, args.len() as u32, args)
}

/// `text` as an Erlang string (a char list) on the runtime's heap.
fn charlist(runtime: &mut Runtime, text: &str) -> Eterm {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    context.charlist(text).expect("a char list")
}

/// The atom `name`, interned in the runtime's shared atom table.
fn atom(runtime: &mut Runtime, name: &str) -> Eterm {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    context.atom(name).expect("an atom")
}

/// Opens `{spawn_driver, command}` with empty options through the runtime's own `open_port/2`.
fn open_spawn_driver(runtime: &mut Runtime, command: &str) -> Eterm {
    let name = {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        let command = context.charlist(command).expect("a command");
        let tag = context.atom("spawn_driver").expect("the spawn_driver atom");
        terms::make_tuple(&mut context, &[tag, command]).expect("a name tuple")
    };
    call(runtime, "erlang", "open_port", &[name, NIL]).expect("open_port")
}

/// `erl_ddll:try_load(<dir>, <name>, [])` — the load result term.
fn try_load(runtime: &mut Runtime, dir: &str, name: &str) -> Eterm {
    let path = charlist(runtime, dir);
    let name = charlist(runtime, name);
    call(runtime, "erl_ddll", "try_load", &[path, name, NIL]).expect("a load result")
}

/// The elements of `term`, pinned to `arity` (a test-local equivalent of a `{Tag, …}` match).
fn tuple_of(term: Eterm, arity: usize) -> Vec<Eterm> {
    assert_eq!(terms::tuple_arity(term).expect("a tuple"), arity, "{term:?}");
    (0..arity)
        .map(|i| terms::tuple_element(term, i).expect("an element"))
        .collect()
}

/// The name of an atom term (a wrong term fails the test rather than being guessed).
fn atom_text(runtime: &Runtime, term: Eterm) -> String {
    runtime
        .atom_name_of(term)
        .unwrap_or_else(|| panic!("{term:?} is not an interned atom"))
        .to_string()
}

/// An Erlang char list term as a Rust `String`.
fn charlist_text(term: Eterm) -> String {
    terms::list_elements(term)
        .expect("a char list")
        .iter()
        .map(|character| {
            let byte = u8::try_from(signed_value(*character)).expect("a byte");
            char::from(byte)
        })
        .collect()
}

/// Every name `erl_ddll:loaded_drivers/0` currently reports, in load order.
fn loaded_names(runtime: &mut Runtime) -> Vec<String> {
    let out = call(runtime, "erl_ddll", "loaded_drivers", &[]).expect("loaded_drivers");
    let elements = tuple_of(out, 2);
    assert_eq!(atom_text(runtime, elements[0]), "ok");
    terms::list_elements(elements[1])
        .expect("a list")
        .iter()
        .map(|name| charlist_text(*name))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Direction (ii): a driver opened by a port is listed
// ---------------------------------------------------------------------------------------------

/// `open_port({spawn_driver, <object path>}, [])` `dlopen`s the object and records its **declared**
/// `driver_name` (the object's file stem) in the loaded-driver list `erl_ddll:loaded_drivers/0`
/// reports — the name was never `try_load`ed, so before the unification the two views disagreed.
///
/// Discriminates: a fresh list is `{ok, []}` here (asserted first), so the name appearing afterwards
/// can only have come from the port's own `dlopen`.
///
/// The oracle (OTP 29.1.1 / erts-17.1, measured) differs on both counts and the differences are
/// asserted rather than closed: its fresh list is
/// `{ok, ["tcp_inet","udp_inet","ram_file_drv"]}` (its statically linked drivers), and
/// `open_port({spawn_driver, "no_such_driver_xyz"}, [])` for a name it has not loaded is `badarg`
/// (`io.c:599-628` looks the name up in its loaded list), where this runtime auto-`dlopen`s the path.
#[test]
fn an_open_port_spawn_driver_is_listed_by_erl_ddll_loaded_drivers() {
    let Some(fixture) = build_fixture("rt_ddll_open") else {
        return;
    };
    let mut runtime = runtime();
    // Divergence: the runtime links no built-in drivers, so a fresh list is empty (the oracle lists
    // its statically linked `tcp_inet`/`udp_inet`/`ram_file_drv`).
    assert!(
        loaded_names(&mut runtime).is_empty(),
        "a fresh loaded-driver list is empty here"
    );

    // The **path form**: a `/`-containing name *is* the object path, and `driver_lookup` derives the
    // driver name from its file stem. The object was never `try_load`ed.
    let port = open_spawn_driver(&mut runtime, &fixture.object_path());
    assert!(
        PortId::from_eterm(port).is_ok(),
        "open_port yields a port term: {port:?}"
    );

    assert_eq!(
        loaded_names(&mut runtime),
        vec![fixture.name.clone()],
        "the port-opened driver's declared driver_name is now listed"
    );

    let out = call(&mut runtime, "erlang", "port_close", &[port]).expect("port_close");
    assert_eq!(atom_text(&mut runtime, out), "true");

    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// Direction (i): a driver try_loaded by erl_ddll is openable by name
// ---------------------------------------------------------------------------------------------

/// `erl_ddll:try_load/3` reports `{ok, loaded}` then `{ok, already_loaded}`, lists the name, and
/// `open_port({spawn_driver, Name}, [])` then succeeds **by that name** — the port re-uses the
/// object path `try_load` recorded.
///
/// Discriminates: the bare name carries no `/`, so without the recorded path the port would search
/// `Name`/`Name.so`/`./Name` and fail — the successful open can only have come from the shared list.
#[test]
fn a_try_loaded_driver_is_listed_and_opens_by_name() {
    let Some(fixture) = build_fixture("rt_ddll_load") else {
        return;
    };
    let mut runtime = runtime();

    let out = try_load(&mut runtime, &fixture.dir_path(), &fixture.name);
    let elements = tuple_of(out, 2);
    assert_eq!(atom_text(&mut runtime, elements[0]), "ok");
    assert_eq!(atom_text(&mut runtime, elements[1]), "loaded");

    // A repeat load of the same name is `already_loaded`, not a second entry.
    let out = try_load(&mut runtime, &fixture.dir_path(), &fixture.name);
    let elements = tuple_of(out, 2);
    assert_eq!(atom_text(&mut runtime, elements[0]), "ok");
    assert_eq!(atom_text(&mut runtime, elements[1]), "already_loaded");

    assert_eq!(
        loaded_names(&mut runtime),
        vec![fixture.name.clone()],
        "the try_loaded name is listed exactly once"
    );

    // The cross-view direction: the name *is* openable as `{spawn_driver, Name}`.
    let port = open_spawn_driver(&mut runtime, &fixture.name);
    assert!(
        PortId::from_eterm(port).is_ok(),
        "open_port by name yields a port term: {port:?}"
    );
    assert_eq!(
        loaded_names(&mut runtime),
        vec![fixture.name.clone()],
        "opening by name keeps the single recorded entry"
    );

    let out = call(&mut runtime, "erlang", "port_close", &[port]).expect("port_close");
    assert_eq!(atom_text(&mut runtime, out), "true");

    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// `erl_ddll:info/2`
// ---------------------------------------------------------------------------------------------

/// `erl_ddll:info/2` answers for a **listed** name and is `badarg` for an unknown name or item — a
/// listed name is one the shared list holds, so this too reads the unified view.
#[test]
fn info_answers_for_a_listed_name_and_badarg_otherwise() {
    let Some(fixture) = build_fixture("rt_ddll_info") else {
        return;
    };
    let mut runtime = runtime();
    let out = try_load(&mut runtime, &fixture.dir_path(), &fixture.name);
    assert_eq!(atom_text(&mut runtime, tuple_of(out, 2)[1]), "loaded");

    // `port_count` of a freshly loaded driver with no port open is `0`.
    let name = charlist(&mut runtime, &fixture.name);
    let item = atom(&mut runtime, "port_count");
    let out = call(&mut runtime, "erl_ddll", "info", &[name, item]).expect("info");
    assert_eq!(signed_value(out), 0);

    // A dynamically loaded driver is neither statically linked nor permanent.
    let name = charlist(&mut runtime, &fixture.name);
    let item = atom(&mut runtime, "linked_in_driver");
    let out = call(&mut runtime, "erl_ddll", "info", &[name, item]).expect("info");
    assert_eq!(atom_text(&mut runtime, out), "false");

    // `driver_options` is `[]` for a load that did not pass `kill_ports`.
    let name = charlist(&mut runtime, &fixture.name);
    let item = atom(&mut runtime, "driver_options");
    let out = call(&mut runtime, "erl_ddll", "info", &[name, item]).expect("info");
    assert!(terms::list_elements(out).expect("a list").is_empty());

    // An unknown **name** is `badarg` whatever the item.
    let name = charlist(&mut runtime, "no_such_driver_rt_ddll");
    let item = atom(&mut runtime, "port_count");
    assert!(matches!(
        call(&mut runtime, "erl_ddll", "info", &[name, item]),
        Err(BeamError::BadArg)
    ));

    // An unknown **item** on a listed name is `badarg` too.
    let name = charlist(&mut runtime, &fixture.name);
    let item = atom(&mut runtime, "no_such_item_rt_ddll");
    assert!(matches!(
        call(&mut runtime, "erl_ddll", "info", &[name, item]),
        Err(BeamError::BadArg)
    ));

    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// `erl_ddll:try_unload/2`
// ---------------------------------------------------------------------------------------------

/// `erl_ddll:try_unload/2` is `ok`, the name leaves the list, and a repeat is
/// `{error, not_loaded}` — the removal reads and writes the one shared list.
///
/// The unload is the documented immediate divergence: the oracle (measured on a dynamically loaded
/// driver) answers `{ok, pending_process}` while a port still holds the driver and
/// `{ok, pending_driver}` afterwards — and keeps the name **listed** in both cases — where this
/// runtime removes the entry at once. Both of those answers are asserted here as the divergence they
/// are, never silently matched to the oracle's.
#[test]
fn try_unload_removes_the_name_and_a_repeat_is_not_loaded() {
    let Some(fixture) = build_fixture("rt_ddll_unload") else {
        return;
    };
    let mut runtime = runtime();
    let out = try_load(&mut runtime, &fixture.dir_path(), &fixture.name);
    assert_eq!(atom_text(&mut runtime, tuple_of(out, 2)[1]), "loaded");
    assert_eq!(loaded_names(&mut runtime), vec![fixture.name.clone()]);

    let name = charlist(&mut runtime, &fixture.name);
    let out = call(&mut runtime, "erl_ddll", "try_unload", &[name, NIL]).expect("an unload result");
    assert_eq!(atom_text(&mut runtime, out), "ok");
    assert!(
        loaded_names(&mut runtime).is_empty(),
        "the unloaded name leaves the list"
    );

    let name = charlist(&mut runtime, &fixture.name);
    let out = call(&mut runtime, "erl_ddll", "try_unload", &[name, NIL]).expect("an unload result");
    let elements = tuple_of(out, 2);
    assert_eq!(atom_text(&mut runtime, elements[0]), "error");
    assert_eq!(atom_text(&mut runtime, elements[1]), "not_loaded");

    fixture.cleanup();
}
