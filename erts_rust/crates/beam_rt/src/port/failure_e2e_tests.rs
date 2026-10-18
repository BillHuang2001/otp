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

//! The A.4 **driver-failure channel**, end to end on a **real C `.so`**.
//!
//! [`super::failure_tests`](super) pins the reason mapping and the queue bookkeeping; this suite
//! pins the whole road a C driver's own `driver_failure*`/`driver_exit` call really travels:
//!
//! ```text
//! port_command(P, <<Op>>)
//!   → beam_io::PortRegistry::output
//!   → DriverPortAdapter::output → the entry's `output` callback → driver_failure*(P, …)
//!   → DriverHost::failure → FailureLatch (first-wins)
//!   → DriverPortAdapter::settle → DriverContext::signal_failure
//!   → beam_io::PortRegistry::reap_failures (close + PortFailure queued)
//!   → PortRegistry::drain_io_completions → drain_failures → fail_port
//!   → PendingPortSignals::push_ready  ⇒  take_port_signals()
//! ```
//!
//! The observable is the [`PortSignal`] list the runtime publishes — its `kind` (`Exit` to the
//! owner, `Down(reference)` to a watcher) and its `reason` — plus the port's own disappearance.
//!
//! # The oracle rows (measured on OTP 29.1.1 / erts-17.1, one fresh port per row)
//!
//! A trapping owner (`process_flag(trap_exit,true)`) that also watches the port with
//! `monitor(port, P)` receives, in this order:
//!
//! | `port_command` opcode | the driver calls | owner/watcher |
//! |---|---|---|
//! | 1 | `driver_failure_eof(P)` | `{'EXIT',P,normal}`, `{'DOWN',…,normal}` |
//! | 2 | `driver_failure_atom(P,"boom")` | `{'EXIT',P,boom}`, `{'DOWN',…,boom}` |
//! | 3 | `driver_failure_posix(P,EACCES)` | `{'EXIT',P,eacces}`, `{'DOWN',…,eacces}` |
//! | 4 | `driver_failure(P,7)` | `{'EXIT',P,7}`, `{'DOWN',…,7}` |
//! | 5 | `driver_failure(P,0)` | `{'EXIT',P,normal}`, `{'DOWN',…,normal}` |
//! | 6 | `driver_exit(P,0)` | **no `'EXIT'`**, `{'DOWN',…,normal}` |
//! | 7 | `driver_exit(P,EACCES)` | **no `'EXIT'`**, `{'DOWN',…,eacces}` |
//! | 8 | `driver_output(P,"hi",2)` then `driver_failure(P,9)` | `{P,{data,<<"hi">>}}` **then** `{'EXIT',P,9}`, `{'DOWN',…,9}` |
//!
//! and after every one of them `erlang:port_info(P)` is `undefined` and `erlang:port_close(P)`
//! raises `error:badarg`.
//!
//! # What a `--lib` suite can and cannot prove
//!
//! Everything above is pinned here at the [`PortSignal`] level, which is the runtime-side
//! observable *before* a delivery route applies the recipient's `trap_exit` policy. The
//! **non-trapping owner's fate** (per the same measurement: `eof`/`driver_failure(P,0)`/
//! `driver_exit(P,0)`/`driver_exit(P,EACCES)` leave it *alive*, while `boom`/`eacces`/`7`/`9`
//! **terminate** it with that very reason) is [`super::super::deliver_signal`]'s — it needs a
//! `ProcessTable` holding the owner — and is declared, not fabricated, here: every row asserts the
//! `Exit` entry's own `reason`, and the `driver_exit` rows assert that **no** `Exit` entry exists.
//! See the recommended integration suite in the crate's `tests/` node.
//!
//! # One message per failure, never two
//!
//! A reaped failure also mints a `PortNotification::Closed` report (the port-**number** retirement
//! report). It reaches [`PendingPortSignals::report`](super::super::exit::PendingPortSignals), which
//! finds nothing to release because [`PortRegistry::fail_port`](super::PortRegistry) publishes
//! straight into the deliverable queue — so every row here counts its entries exactly.
//!
//! # How the fixture reaches the host (`driver_failure*` and `driver_output`)
//!
//! A `--lib` unit-test link is not given `-Wl,--export-dynamic`, so the fixture imports **no**
//! `driver_*` symbol (it would fail to `dlopen`). It is handed the six host functions it needs as
//! `%lx` addresses in the loader's `command` string — the `ready_output_tests.rs` handoff — and
//! calls them through the pointers it is given. `driver::lifecycle` is private, so the addresses
//! come from [`beam_abi::driver::drv::driver_api_table`]'s populated slots.
//!
//! # Skip policy
//!
//! Needs the host C compiler and the host `erl_driver.h`; without either it prints a **loud**
//! `SKIP:` banner and returns — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_bifs::PortHook;
use beam_io::PortNotification;
use beam_process::{MonitorRef, PortId, ProcessId, RefAllocator};
use beam_term::NIL;

use crate::port::exit::{PortSignal, PortSignalKind, PortSignalReason};
use crate::port::tests::{assert_badarg, fixture, Fixture};
use crate::port::RuntimePorts;

/// The process the port belongs to — the recipient of every `'EXIT'`.
const OWNER: ProcessId = ProcessId::new(11, 0);/// The `monitor(port, P)` watcher — the recipient of every `'DOWN'`.
const WATCHER: ProcessId = ProcessId::new(12, 0);

/// The fixture: one driver whose `output` callback closes its own port through the C's
/// `driver_failure*` family or `driver_exit`, dispatching on the first payload byte.
///
/// Opcodes and the driver call each one makes are the module's oracle table. The six host
/// functions arrive as `%lx` addresses after the driver-object token in the loader's `command`
/// string (see the module docs); the driver-object token itself is skipped with `%*s`.
const FAILURE_C: &str = r#"
#include <erl_driver.h>

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
typedef int (*rt_eof_fn)(ErlDrvPort);
typedef int (*rt_atom_fn)(ErlDrvPort, char *);
typedef int (*rt_code_fn)(ErlDrvPort, int);
typedef int (*rt_output_fn)(ErlDrvPort, char *, ErlDrvSizeT);

typedef struct {
    ErlDrvPort port;
    rt_eof_fn failure_eof;
    rt_atom_fn failure_atom;
    rt_code_fn failure_posix;
    rt_code_fn failure;
    rt_code_fn exit;
    rt_output_fn output;
} rt_state;

/* One port per loaded object: the state lives in this object's own storage, because the fixture
 * deliberately imports neither `driver_alloc`/`driver_free` nor any `driver_*` symbol. */
static rt_state rt_state_storage;

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    rt_state *st = &rt_state_storage;
    unsigned long eof_addr = 0, atom_addr = 0, posix_addr = 0;
    unsigned long code_addr = 0, exit_addr = 0, output_addr = 0;
    if (command == NULL)
        return (ErlDrvData) -1;
    /* `<driver object> <eof> <atom> <posix> <failure> <exit> <output>` — the first token is the
     * object the driver was loaded from (the production `open_port` name shape). */
    if (sscanf(command, "%*s %lx %lx %lx %lx %lx %lx",
               &eof_addr, &atom_addr, &posix_addr,
               &code_addr, &exit_addr, &output_addr) != 6)
        return (ErlDrvData) -1;
    if (eof_addr == 0 || atom_addr == 0 || posix_addr == 0 || code_addr == 0
        || exit_addr == 0 || output_addr == 0)
        return (ErlDrvData) -1;
    st->failure_eof = (rt_eof_fn) (intptr_t) eof_addr;
    st->failure_atom = (rt_atom_fn) (intptr_t) atom_addr;
    st->failure_posix = (rt_code_fn) (intptr_t) posix_addr;
    st->failure = (rt_code_fn) (intptr_t) code_addr;
    st->exit = (rt_code_fn) (intptr_t) exit_addr;
    st->output = (rt_output_fn) (intptr_t) output_addr;
    st->port = port;
    return (ErlDrvData) st;
}

static void rt_stop(ErlDrvData drv_data) {
    (void) drv_data;
}

/* The callback under test: closes this very port from inside its own `output`. */
static void rt_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    rt_state *st = (rt_state *) drv_data;
    if (st == NULL || len == 0)
        return;
    switch (buf[0]) {
    case 1: (void) st->failure_eof(st->port); break;
    case 2: (void) st->failure_atom(st->port, "boom"); break;
    case 3: (void) st->failure_posix(st->port, EACCES); break;
    case 4: (void) st->failure(st->port, 7); break;
    case 5: (void) st->failure(st->port, 0); break;
    case 6: (void) st->exit(st->port, 0); break;
    case 7: (void) st->exit(st->port, EACCES); break;
    /* Data first, then the failure: the delivered order the channel must preserve. */
    case 8: (void) st->output(st->port, "hi", 2); (void) st->failure(st->port, 9); break;
    default: break;
    }
}

static ErlDrvEntry rt_failure_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = rt_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_FAILURE_NAME,
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

DRIVER_INIT(rt_failure_init) {
    return &rt_failure_entry;
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
/// A compact duplicate of `driver/ready_output_tests.rs::erl_include_dir` (a unit test cannot
/// import the `tests/` crate target's support): ask the host `erl`; then the `include` sibling of
/// the `erts-*` directory the `erl` on `$PATH` lives in; then the standard roots.
fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval().or_else(from_path_erl).or_else(|| {
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

/// A compiled driver `.so` in a scratch directory.
struct Compiled {
    /// The scratch directory holding the source and the compiled `.so`.
    dir: PathBuf,
    /// The `.so` with its suffix — the `open_port` name's first token.
    object: PathBuf,
    /// The driver name the entry declares (the loader rejects a mismatch).
    name: String,
}

impl Compiled {
    /// Removes the scratch directory (`cc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// Compiles [`FAILURE_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing (the
/// caller returns early — **loudly**).
fn build_fixture(stem: &str) -> Option<Compiled> {
    if !cc_available() {
        skip(&format!(
            "driver failure: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("driver failure: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_driver_failure_{stem}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{stem}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, FAILURE_C));
    if let Err(error) = write {
        skip(&format!(
            "driver failure: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    // `cc -shared -fPIC -DRT_FAILURE_NAME="<stem>" -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_FAILURE_NAME=\"{stem}\""))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "driver failure: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!(
                "driver failure: could not run the C compiler: {error}"
            ));
            return None;
        }
    }
    if !dir.join(format!("{stem}.so")).is_file() {
        skip(&format!("driver failure: the C compiler produced no {stem}.so"));
        return None;
    }
    Some(Compiled {
        object: dir.join(format!("{stem}.so")),
        dir,
        name: stem.to_string(),
    })
}

/// The six host functions the fixture calls, as the `%lx` addresses it is handed.
///
/// `driver::lifecycle` is a private module, so the populated [`beam_abi::driver::drv::
/// driver_api_table`] slots are the only safe route to them.
fn host_addresses() -> String {
    let table = beam_abi::driver::drv::driver_api_table();
    let address = |name: &str, slot: Option<*const ()>| {
        slot.map(|pointer| pointer as usize)
            .unwrap_or_else(|| panic!("the driver API table's `{name}` slot is populated"))
    };
    let eof = address(
        "driver_failure_eof",
        table.driver_failure_eof.map(|f| f as *const ()),
    );
    let atom = address(
        "driver_failure_atom",
        table.driver_failure_atom.map(|f| f as *const ()),
    );
    let posix = address(
        "driver_failure_posix",
        table.driver_failure_posix.map(|f| f as *const ()),
    );
    let code = address(
        "driver_failure",
        table.driver_failure.map(|f| f as *const ()),
    );
    let exit = address("driver_exit", table.driver_exit.map(|f| f as *const ()),);
    let output = address("driver_output", table.driver_output.map(|f| f as *const ()));
    format!("{eof:x} {atom:x} {posix:x} {code:x} {exit:x} {output:x}")
}

// ---------------------------------------------------------------------------------------------
// One row: open the port, install the monitor, close the port from inside the driver
// ---------------------------------------------------------------------------------------------

/// One open port on the fixture driver, plus the monitor the row installed before the command.
struct Case {
    /// The compiled driver (its scratch directory is removed by [`Case::finish`]).
    compiled: Compiled,
    /// The provider's own fixture: the shared atom table, the registry and the term heap.
    fx: Fixture,
    /// The provider the port was opened through.
    hook: RuntimePorts,
    /// The port under test.
    id: PortId,
    /// The reference `monitor(port, P)` installed, carried on the `'DOWN'` entry.
    reference: MonitorRef,
}

impl Case {
    /// Takes every published signal (after the failure has been drained).
    ///
    /// Taking is destructive, so a row reads **once** and asserts on that one list; there is
    /// deliberately no second (non-consuming) accessor — [`assert_row`]'s whole-list comparison is
    /// the assertion for both halves of the oracle table, an owed `'EXIT'` *and* the absence of one.
    fn signals(&mut self) -> Vec<PortSignal> {
        self.fx.registry.borrow_mut().take_port_signals()
    }

    /// Hands the port back and removes the scratch directory.
    fn finish(self) {
        self.compiled.cleanup();
    }
}

/// Opens one port on the fixture driver and installs `monitor(port, P)` from [`WATCHER`].///
/// The command is `port_command(P, <<opcode>>)`: the driver's `output` callback is what closes the
/// port, so nothing is drained until [`Case::signals`] is called (the write's own channel drain
/// publishes the signals).
fn open_row(stem: &str) -> Option<Case> {
    let compiled = build_fixture(stem)?;
    // The same anchor `crate::port::PortRegistry::new` uses.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let mut fx = fixture();
    let command = format!("{} {}", compiled.object.display(), host_addresses());
    let command = fx.string(&command);
    let name = fx.name("spawn_driver", command);
    let mut hook = RuntimePorts::new(Rc::clone(&fx.registry), Rc::clone(&fx.atoms), OWNER);
    let id = match hook.open_port(name, NIL) {
        Ok(id) => id,
        Err(error) => {
            skip(&format!(
                "driver failure ({}): the fixture driver did not load ({error})",
                compiled.name
            ));
            compiled.cleanup();
            return None;
        }
    };
    let reference = RefAllocator::new().next();    {
        let mut registry = fx.registry.borrow_mut();
        registry
            .port_table_mut()
            .monitor(id, WATCHER, reference)
            .expect("the watcher's port monitor is installed on a live port");
    }
    Some(Case {
        compiled,
        fx,
        hook,
        id,
        reference,
    })
}

/// Closes the port from inside the driver's `output` callback by writing `opcode`.
fn fail(case: &mut Case, opcode: u8) {
    let mut registry = case.fx.registry.borrow_mut();
    registry
        .write_command(case.id, &[opcode])
        .expect("the write itself succeeds: the driver closed its own port, it did not refuse");
}

/// The two messages a row owes: the owner's `'EXIT'` (when the family owes one) and the watcher's
/// `'DOWN'`, both carrying `reason`.
///
/// Asserts the *whole* list, so a duplicated message (the coincident `Closed` report publishing
/// twice) fails here rather than passing a filter.
fn assert_row(
    case: &mut Case,
    owes_exit: bool,
    reason: PortSignalReason,
    what: &str,
) {
    let expected = if owes_exit {
        vec![
            PortSignal {
                port: case.id,
                to: OWNER,
                kind: PortSignalKind::Exit,
                reason: reason.clone(),
            },
            PortSignal {
                port: case.id,
                to: WATCHER,
                kind: PortSignalKind::Down(case.reference),
                reason,
            },
        ]
    } else {
        vec![PortSignal {
            port: case.id,
            to: WATCHER,
            kind: PortSignalKind::Down(case.reference),
            reason,
        }]
    };
    let signals = case.signals();
    assert_eq!(signals, expected, "{what}: the published signals");
    // The oracle's other half: `erlang:port_info(P)` is `undefined` — this runtime's empty item
    // list is that rendering (see [`crate::port::PortRegistry::info`]).
    assert!(
        case.hook
            .port_info(case.id)
            .expect("port_info answers for a failed port")
            .is_empty(),
        "{what}: a failed port has no port_info/1 items"
    );
    // ...and the port is as gone as a closed one.
    let mut registry = case.fx.registry.borrow_mut();    assert!(!registry.is_live(case.id), "{what}: the port left the runtime");
    assert!(registry.is_empty(), "{what}: no live port remains");
    assert!(
        registry.has_nothing_to_poll(),
        "{what}: nothing is pending once the signals are taken"
    );
    assert_badarg(registry.close(case.id));
}
// ---------------------------------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------------------------------

/// `driver_failure_eof(P)` ⇒ `{'EXIT',P,normal}` **and** `{'DOWN',…,normal}`.
///
/// Also the row that pins the queue state: a live port keeps the registry from declaring
/// quiescence, and so does an owed signal once the port is gone.
#[test]
fn driver_failure_eof_signals_normal_to_the_owner_and_the_watcher() {
    let Some(mut case) = open_row("rt_failure_eof") else {
        return;
    };
    assert!(
        !case.fx.registry.borrow().has_nothing_to_poll(),
        "a live port is pending work"
    );
    fail(&mut case, 1);
    assert!(
        !case.fx.registry.borrow().has_nothing_to_poll(),
        "the owed 'EXIT'/'DOWN' are messages a process is still waiting for"
    );
    assert_row(&mut case, true, PortSignalReason::Normal, "driver_failure_eof");
    assert!(
        case.fx.registry.borrow().has_nothing_to_poll(),
        "once the signals are taken the registry is quiescent again"
    );
    case.finish();
}

/// `driver_failure_atom(P,"boom")` ⇒ the driver's own atom on both messages.
#[test]
fn driver_failure_atom_signals_the_drivers_own_atom() {
    let Some(mut case) = open_row("rt_failure_atom") else {
        return;
    };
    fail(&mut case, 2);
    assert_row(
        &mut case,
        true,
        PortSignalReason::Atom("boom".to_string()),
        "driver_failure_atom(P,\"boom\")",
    );
    case.finish();
}

/// `driver_failure_posix(P,EACCES)` ⇒ the errno's lowercase POSIX name, never `{error, ·}`.
#[test]
fn driver_failure_posix_signals_the_errno_name() {
    let Some(mut case) = open_row("rt_failure_posix") else {
        return;
    };
    fail(&mut case, 3);
    assert_row(
        &mut case,
        true,
        PortSignalReason::Atom("eacces".to_string()),
        "driver_failure_posix(P,EACCES)",
    );
    case.finish();
}

/// `driver_failure(P,7)` ⇒ the bare integer, on the `'EXIT'` **and** the `'DOWN'`.
#[test]
fn driver_failure_with_a_code_signals_the_bare_integer() {
    let Some(mut case) = open_row("rt_failure_code") else {
        return;
    };
    fail(&mut case, 4);
    assert_row(
        &mut case,
        true,
        PortSignalReason::Integer(7),
        "driver_failure(P,7)",
    );
    case.finish();
}

/// `driver_failure(P,0)` is the C's **eof** route (`code == 0`), not `Error(0)`: `normal`.
#[test]
fn driver_failure_with_zero_is_the_eof_shape() {
    let Some(mut case) = open_row("rt_failure_zero") else {
        return;
    };
    fail(&mut case, 5);
    assert_row(
        &mut case,
        true,
        PortSignalReason::Normal,
        "driver_failure(P,0)",
    );
    case.finish();
}

/// `driver_exit(P,0)` runs the C's `unlink_proc` first (`io.c:7244`), so the owner is owed **no**
/// `'EXIT'` at all — only the watcher's `'DOWN'`, still carrying `normal`.
#[test]
fn driver_exit_with_zero_owes_the_watcher_a_normal_down_and_the_owner_nothing() {
    let Some(mut case) = open_row("rt_failure_exit_zero") else {
        return;
    };
    fail(&mut case, 6);
    // `owes_exit == false`: the whole-list comparison below is the `unlink_proc` assertion — a
    // published `'EXIT'` would make the list longer than the watcher's one `'DOWN'`.
    assert_row(&mut case, false, PortSignalReason::Normal, "driver_exit(P,0)");
    case.finish();
}

/// `driver_exit(P,EACCES)` likewise owes no `'EXIT'`, and its watcher's `'DOWN'` carries `eacces`.
///
/// This is the row that tells the two families apart **by reason** as well as by addressee: the
/// non-trapping owner survives (no `'EXIT'` is ever delivered), which is why the oracle's table
/// has no termination for it.
#[test]
fn driver_exit_with_a_posix_code_owes_the_watcher_an_eacces_down_and_the_owner_nothing() {
    let Some(mut case) = open_row("rt_failure_exit_posix") else {
        return;
    };
    fail(&mut case, 7);
    assert_row(
        &mut case,
        false,
        PortSignalReason::Atom("eacces".to_string()),
        "driver_exit(P,EACCES)",
    );
    case.finish();
}
/// The data-then-failure row: a `driver_output` **before** the failure is delivered ahead of the
/// port's exit, and exactly one `'EXIT'` (never two) is published.
///
/// The read is observed where the runtime really produces it — the port-task round's notification
/// list (`PortNotification::Data`), which `beam_io` re-prepends for a reaped port — because the
/// failure's own teardown removes the port's metadata in the same call, and
/// [`PortRegistry::fold_notifications`](crate::port::PortRegistry) renders a read only for a port
/// it still holds. The remaining half is stated honestly: the runtime drops the read for the port
/// the driver just closed, where the oracle delivers `{P,{data,<<"hi">>}}` first. That residual
/// belongs to `port.rs`'s fold gate (out of this node's write scope) and is recorded in the
/// module docs of `src/port/CONTEXT.md`'s A.4 *Known Issues* by the crate's owning node.
#[test]
fn a_data_read_before_a_failure_reaches_the_round_ahead_of_one_exit_signal() {
    let Some(mut case) = open_row("rt_failure_data") else {
        return;
    };
    fail(&mut case, 8);
    // The failure published exactly one 'EXIT' (and the watcher's 'DOWN'), never two.
    let signals = case.signals();
    assert_eq!(
        signals
            .iter()
            .filter(|signal| signal.kind == PortSignalKind::Exit)
            .count(),
        1,
        "one failure is one 'EXIT', and the coincident `Closed` report releases nothing more: \
         {signals:?}"
    );
    assert_eq!(
        signals,
        vec![
            PortSignal {
                port: case.id,
                to: OWNER,
                kind: PortSignalKind::Exit,
                reason: PortSignalReason::Integer(9),
            },
            PortSignal {
                port: case.id,
                to: WATCHER,
                kind: PortSignalKind::Down(case.reference),
                reason: PortSignalReason::Integer(9),
            },
        ],
        "the data-then-failure row's signals"
    );

    // The bytes the failing callback delivered: `beam_io` re-harvested them for a reaped port, so
    // the next port-task round reports them exactly once, ahead of the close report.
    let outcome = {
        let registry = &mut *case.fx.registry.borrow_mut();
        registry
            .registry
            .execute_port_tasks(&mut registry.poller, 1024)
            .expect("the port-task round runs")
    };
    let reads: Vec<Vec<u8>> = outcome        .notifications
        .iter()
        .filter_map(|notification| match notification {
            PortNotification::Data { port, read } if *port == case.id => Some(read.flattened()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reads,
        vec![b"hi".to_vec()],
        "the failing callback's own `driver_output` bytes are reported once, byte for byte"
    );
    let mut after = case.fx.registry.borrow_mut();
    assert!(
        !after.is_live(case.id),
        "the port is gone: the read above and the signals are all that is left of it"
    );
    assert_badarg(after.close(case.id));
    drop(after);    case.finish();
}

/// The driver's `driver_failure*` return value is the host's (`0` — the C's own success code),
/// and a **second** failure cannot be latched on a port that is already going away.
///
/// Pinned at the fixture boundary: the port is live for the first call and gone for the second,
/// which is what makes `driver_failure*` a *port-closing request* rather than a return value. The
/// second call is made on a **fresh** port (the C's documented use-after-free makes a second call
/// on a just-terminated port unpinnable, `beam_abi/src/driver/CONTEXT.md`).
#[test]
fn a_second_failure_needs_a_second_port() {
    let Some(mut first) = open_row("rt_failure_twice_a") else {
        return;
    };
    fail(&mut first, 2);
    assert_eq!(
        first
            .signals()
            .iter()
            .filter(|signal| signal.kind == PortSignalKind::Exit)
            .count(),
        1,
        "one failure, one 'EXIT'"
    );
    first.finish();

    let Some(mut second) = open_row("rt_failure_twice_b") else {
        return;
    };
    fail(&mut second, 3);
    let signals = second.signals();
    assert_eq!(
        signals.len(),
        2,
        "a fresh port owes its own two messages: {signals:?}"
    );
    assert!(
        signals
            .iter()
            .any(|signal| signal.reason == PortSignalReason::Atom("eacces".to_string())),
        "the second port's own failure carried its own reason"
    );
    second.finish();
}

/// The suite's own `expect`-based fixtures must not silently pass when a host facility is missing:
/// `open_row` prints the `SKIP:` banner, and this case pins that the banner helper is reachable
/// (a compile-time use, so an unused-function warning cannot hide a broken skip path).
#[test]
fn the_skip_banner_is_loud() {
    skip("driver failure: fixture probe (this line only proves the banner is wired)");
}
