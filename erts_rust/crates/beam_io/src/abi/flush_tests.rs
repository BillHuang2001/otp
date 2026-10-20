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
//! Path-included tests for [`super::DriverPort`]'s **`flush`** forwarding — the loaded C driver's
//! one-chance-before-`stop` callback (the C's `drv_ptr->flush`, `flush_port`,
//! `erts/emulator/beam/io.c:3659-3700`).
//!
//! A sibling `#[path]` child of `port` (declared at the bottom of `port.rs`, like `host_tests.rs`
//! is of `host`), so `super::*` still reaches the module's private items while `port.rs` stays near
//! the workspace's ~1000-line convention.
//!
//! These are **real** `dlopen`ed C drivers, not fakes: [`crate::abi::fixtures::flush_driver.c`]
//! (the entry that declares `flush`) and its `flush_driver_no_flush.c` variant (the entry that
//! leaves the slot `NULL`), compiled by this suite with the system `cc` against the system
//! `erl_driver.h`. They are the only way to pin the adapter, because the two facts under test —
//! "the entry's own callback ran" and "a `NULL` slot is not an error" — live on the far side of the
//! C ABI.
//!
//! What is pinned:
//!
//! * the loaded driver's `flush` runs **exactly once**, **before** its `stop`;
//! * what it does from inside the callback reaches the port: its `driver_output` marker becomes one
//!   read message on the ordinary sink path (`PortNotification::Data` is minted from it). That leg
//!   needs the fixture to call a host function, and a `driver_*` symbol only resolves out of the
//!   loading binary's **dynamic** symbol table — which the library's own unit-test binary does not
//!   have — so it runs from the marker build `flush_driver_marker.c` and probes first; see
//!   [`host_symbols_exported`];
//! * an entry with **no** `flush` slot is the C's silent no-op (`io.c:3666`): the close is `Ok`, no
//!   error escapes from the loader's `absent("flush")`, and the driver's `stop` still runs;
//! * the close-time order is `flush` → the port's own write drain (which reaches the driver's
//!   `output`) → `stop`;
//! * a loaded driver **never parks** a closing port: its `output`/`outputv` are `void`, so the port
//!   counts the whole queued command as written (the documented deviation on
//!   [`crate::Driver::output`]), which empties the queue inside the close. The parking half of the
//!   flush-then-close shape belongs to the Rust drivers and is pinned by `src/closing_tests.rs`.
//!
//! Skip-vs-fail policy: compiling the fixtures needs a C compiler and the system OTP include
//! directory; when either is missing every test prints a loud `SKIP:` banner and returns (never a
//! silent pass, never a failure). Two things are conditional rather than fail-worthy, and both are
//! probed rather than assumed:
//!
//! * the one fixture that makes host calls (the marker build) cannot `dlopen` where the loading
//!   binary does not export the `driver_*` symbols — `host_symbols_exported` decides, and the test
//!   prints the precise reason instead of failing;
//! * the base builds make **no** host calls at all, so they load in either binary and the ordering
//!   facts are pinned everywhere. A load failure there is not a skip: it is a wiring bug.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use super::DriverPort;
use crate::abi::LocalAtomPortHost;
use crate::poller::Poller;
use crate::port_task::PortNotification;
use crate::PortRegistry;
// ---------------------------------------------------------------------------------------------
// The fixture: building it with the system `cc`
// ---------------------------------------------------------------------------------------------
/// The `erl_driver.h` filename every include-directory candidate must hold.
const HEADER: &str = "erl_driver.h";
/// The committed fixture directory.
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/abi/fixtures")
}
/// The entry that **declares** a `flush` slot.
fn flush_fixture_source() -> PathBuf {
    fixture_dir().join("flush_driver.c")
}
/// The entry that leaves `flush` `NULL` (the C's no-op shape).
fn no_flush_fixture_source() -> PathBuf {
    fixture_dir().join("flush_driver_no_flush.c")
}
/// The entry whose `flush`/`output` additionally hand bytes up through `driver_output` — the only
/// build that needs the loading binary to export the `driver_*` host symbols.
fn marker_fixture_source() -> PathBuf {
    fixture_dir().join("flush_driver_marker.c")
}
/// Whether this test binary's **dynamic** symbol table exports the `driver_*` host symbols, i.e.
/// whether a `dlopen`ed driver can resolve them out of the loading process.
///
/// A driver `.so` leaves `driver_*` undefined, exactly like a real OTP driver, and `dlopen(RTLD_NOW)`
/// resolves them from the process's global symbol scope — the same scope `dlsym(RTLD_DEFAULT, …)`
/// searches, so this is the precise probe. `-Wl,--export-dynamic` (which `build.rs` adds to the
/// *integration* test targets) is what puts them there; the library's own unit-test binary does not
/// get it, so the marker fixture asks this first instead of failing on a `dlopen` it cannot win.
fn host_symbols_exported() -> bool {
    // SAFETY: `dlsym` is handed a valid NUL-terminated name and `RTLD_DEFAULT`, the process-wide
    // scope handle the dynamic linker defines. It answers either a symbol address or NULL; the
    // address is only tested for null here and never dereferenced.
    let sym = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            b"driver_output\0".as_ptr().cast::<libc::c_char>(),
        )
    };
    !sym.is_null()
}
/// Compile `src` into `<scratch>/<stem>.so`, or print a loud `SKIP:` and answer `None` when the
/// toolchain is unavailable.
fn compile_fixture(src: &Path, stem: &str) -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip("abi::flush: no OTP include directory (erl_driver.h) found");
        return None;
    };
    if !cc_available() {
        skip("abi::flush: no C compiler (cc / $CC) available");
        return None;
    }
    assert!(
        src.is_file(),
        "the committed fixture is missing: {}",
        src.display()
    );
    let dir = scratch_dir(stem);
    std::fs::create_dir_all(&dir).expect("a scratch directory for the fixture");
    let so = dir.join(format!("{stem}.so"));
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(&so)
        .arg(src)
        .output()
        .expect("the C compiler could not be run");
    assert!(
        output.status.success(),
        "the {stem} fixture must compile into a shared object:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(so)
}
/// The include directory holding the system `erl_driver.h`, resolved the same way the crate's
/// integration suites do it: ask `erl`, then derive it from the `erl` on `PATH`, then probe the
/// standard roots.
fn erl_include_dir() -> Option<PathBuf> {
    from_erl_eval()
        .or_else(from_path_erl)
        .or_else(|| standard_roots().into_iter().find_map(|root| search_root(&root)))
}
/// `code:lib_dir(erts, include)` from the system `erl`.
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
    (dir.join(HEADER).is_file()).then_some(dir)
}
/// `.../erts-<vsn>/bin/erl` on `PATH` → its sibling `.../erts-<vsn>/include`.
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
    (include.join(HEADER).is_file()).then_some(include)
}
/// The installation roots to probe when neither `erl` nor `PATH` answers.
fn standard_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top));
    }
    for base in ["/usr/lib/erlang", "/usr/local/lib/erlang", "/opt/erlang"] {
        roots.push(PathBuf::from(base));
    }
    roots
}
/// The first `<root>/erts-*/include` (newest version dir last-resort: any that has the header)
/// holding `erl_driver.h`.
fn search_root(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("erts-"))
        })
        .map(|erts| erts.join("include"))
        .find(|include| include.join(HEADER).is_file())
}
/// Whether the C compiler (`$CC`, else `cc`) can be run.
fn cc_available() -> bool {
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
/// A **fresh** scratch directory under `$TMPDIR` (unique per stem + process + call): the fixture
/// appends its callback log to a file, so a directory that still held a previous run's file would
/// make an assertion read stale lines.
fn scratch_dir(stem: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "beam_io-abi-flush-{stem}-{}-{n}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    // Created eagerly: the callers use this directory both for a compiled fixture and for the
    // callback **log** the driver appends to, and the fixture's own `fopen(…, "a")` cannot create a
    // parent directory (it would fail with ENOENT and the log would read back empty).
    std::fs::create_dir_all(&dir).expect("a fresh scratch directory");
    dir
}
/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}
// ---------------------------------------------------------------------------------------------
// Opening a port around the fixture
// ---------------------------------------------------------------------------------------------
/// Force the `#[no_mangle]` `driver_*` host symbols into this test binary's **dynamic** symbol
/// table, then load the fixture from `so` under `name` and open a port around it.
///
/// The fixture leaves `driver_*` undefined (exactly like a real OTP driver) and resolves them out
/// of the loading process, so `dlopen(RTLD_NOW)` only succeeds once those symbols are exported:
/// `build.rs` adds `-Wl,--export-dynamic` and naming
/// [`beam_abi::driver::drv::driver_api_table`] — the `#[used]` table holding them — is what pulls
/// `beam_abi`'s `drv` object file into the binary.
fn open_fixture(so: &Path, name: &str, command: &str) -> (Poller, PortRegistry, beam_process::PortId) {
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let path = so.to_str().expect("the fixture path is valid UTF-8");
    // The loader appends the shared-object suffix itself, so it is handed the path without it.
    let path = path.strip_suffix(".so").unwrap_or(path);
    let port = DriverPort::load(path, name, command, Box::new(LocalAtomPortHost::new()))
        .unwrap_or_else(|err| {
            panic!(
                "the {name} fixture could not be loaded: {err:?}\n\
                 an `undefined symbol: driver_*` here means the host symbols are not in this test \
                 binary's dynamic symbol table — check build.rs's -Wl,--export-dynamic and the \
                 driver_api_table() reference above."
            )
        });
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(port))
        .expect("the loaded driver opens as a port");
    (poller, registry, id)
}
/// The callback log the fixture appended, one callback name per line.
fn log_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}
/// The number of `PortNotification::Closed` reports a round carried for `id`.
fn closed_of(outcome: &crate::PortTaskOutcome, id: beam_process::PortId) -> usize {
    outcome
        .notifications
        .iter()
        .filter(|notification| matches!(notification, PortNotification::Closed { port } if *port == id))
        .count()
}
// ---------------------------------------------------------------------------------------------
// The `flush` slot: it runs, once, before `stop`, and its callback reaches the port
// ---------------------------------------------------------------------------------------------
/// The loaded driver's own `flush` entry runs **exactly once** and **before** its `stop`.
///
/// The log is the ordering evidence (`start`, `flush`, `stop` — nothing else, in that order). The port
/// is driven directly (rather than through `PortRegistry::close`) so the assertions still see it after
/// a synchronous close removed it from the live map. The data-path half of this test's name — what
/// `flush` emits from *inside* the callback reaching the port — needs a host call and is
/// `the_marker_a_flush_emitting_from_inside_the_callback_reaches_the_port`'s.
#[test]
fn the_loaded_drivers_flush_runs_once_before_its_stop() {
    let Some(so) = compile_fixture(&flush_fixture_source(), "flush_driver") else {
        return;
    };
    let log = scratch_dir("flush-order").join("callbacks.log");
    let (mut poller, mut registry, id) =
        open_fixture(&so, "flush_driver", &format!("log={}", log.display()));
    assert_eq!(
        log_lines(&log),
        vec!["start".to_string()],
        "only the driver's start callback has run so far"
    );
    let port = registry.get_mut(id).expect("the port is live");
    port.close(&mut poller)
        .expect("the close runs the driver's flush, the drain and its stop");
    let lines = log_lines(&log);
    assert_eq!(
        lines,
        vec!["start".to_string(), "flush".to_string(), "stop".to_string()],
        "the driver sees flush once, before stop; log = {lines:?}"
    );
    assert!(
        port.is_closed(),
        "the flush discarded nothing and the queue was empty, so the port closed"
    );
    // At most once: a repeated close is a no-op (no second flush, no second stop).
    port.close(&mut poller)
        .expect("a repeated close on a closed port is a no-op");
    let lines = log_lines(&log);
    assert_eq!(
        lines,
        vec!["start".to_string(), "flush".to_string(), "stop".to_string()],
        "flush and stop each ran exactly once; log = {lines:?}"
    );
}
// ---------------------------------------------------------------------------------------------
// An absent `flush` slot is the C's silent no-op, not an error
// ---------------------------------------------------------------------------------------------
/// An entry that leaves `flush` `NULL` closes cleanly: no error escapes from the loader's
/// `absent("flush")`, and the driver's `stop` still runs.
///
/// The C's rule is `if (p->drv_ptr->flush != NULL)` (`io.c:3663`) — a driver with nothing to
/// discard simply has no callback, which is exactly [`Driver::flush`]'s own default. The variant
/// fixture (`flush_driver_no_flush.c`) is that entry shape.
#[test]
fn an_entry_without_a_flush_slot_closes_cleanly_and_still_stops() {
    let Some(so) = compile_fixture(&no_flush_fixture_source(), "flush_driver_no_flush") else {
        return;
    };
    let log = scratch_dir("flush-absent").join("callbacks.log");
    let (mut poller, mut registry, id) = open_fixture(
        &so,
        "flush_driver",
        &format!("log={}", log.display()),
    );
    registry
        .close(&mut poller, id)
        .expect("an entry with no flush slot closes without error");
    let lines = log_lines(&log);
    assert_eq!(
        lines,
        vec!["start".to_string(), "stop".to_string()],
        "the absent flush ran nothing and the stop still ran; log = {lines:?}"
    );
    assert!(
        !registry.contains(id),
        "the port finished synchronously and left the live map"
    );
    // Its `Closed` report is the next round's, exactly like a Rust driver's.
    let outcome = registry
        .execute_port_tasks(&mut poller, 1000)
        .expect("the round runs");
    assert_eq!(
        closed_of(&outcome, id),
        1,
        "the finished port reports its close in the next round"
    );
}
// ---------------------------------------------------------------------------------------------
// The close-time order: flush, then the port's write drain, then stop
// ---------------------------------------------------------------------------------------------
/// The close-time order is `flush` → the port's own write drain (which reaches the driver's
/// `output`) → `stop`, and the queued command really goes to the device.
///
/// `flush` returning without discarding anything means "keep these bytes, try to write them", which
/// is what makes the drain run at all; the fixture logs all four callbacks, so the order is the
/// evidence. The echo half — what the drain then hands the driver coming back as a read message — is
/// the marker build's (`the_marker_...` below).
#[test]
fn the_flush_precedes_the_write_drain_which_precedes_the_stop() {
    let Some(so) = compile_fixture(&flush_fixture_source(), "flush_driver") else {
        return;
    };
    let log = scratch_dir("flush-drain").join("callbacks.log");
    let (mut poller, mut registry, id) = open_fixture(
        &so,
        "flush_driver",
        &format!("log={}", log.display()),
    );
    let port = registry.get_mut(id).expect("the port is live");
    port.queue_output(b"ab");
    port.close(&mut poller)
        .expect("the close flushes, drains the queued command and stops the driver");
    let lines = log_lines(&log);
    assert_eq!(
        lines,
        vec![
            "start".to_string(),
            "flush".to_string(),
            "output".to_string(),
            "stop".to_string()
        ],
        "flush, then the write, then stop; log = {lines:?}"
    );
    assert_eq!(port.output_len(), 0, "the drain emptied the output queue");
    assert!(port.is_closed(), "the port finished its close");
}
// ---------------------------------------------------------------------------------------------
// A loaded driver never parks a closing port
// ---------------------------------------------------------------------------------------------
/// A loaded C driver's close **completes synchronously**, so the port can never park in
/// `PortState::Closing` — the half of the flush-then-close shape a `.so` cannot exercise.
///
/// The port layer parks a closing port only when its output queue is still non-empty after the
/// drain, and a loaded driver always empties it: its `output`/`outputv` callbacks are `void`, so the
/// port counts the whole queued command as written (the deviation documented on
/// [`crate::Driver::output`]), however little the driver really wrote. The parking half therefore
/// belongs to the Rust drivers and is pinned by `src/closing_tests.rs`; what is pinned **here** is
/// the loaded-driver consequence — after `PortRegistry::close` the port is gone from the live map,
/// `is_closing` no longer answers for it, and its `Closed` report is the next round's.
#[test]
fn a_loaded_driver_never_parks_a_closing_port() {
    let Some(so) = compile_fixture(&flush_fixture_source(), "flush_driver") else {
        return;
    };
    let log = scratch_dir("flush-park").join("callbacks.log");
    let (mut poller, mut registry, id) = open_fixture(
        &so,
        "flush_driver",
        &format!("log={}", log.display()),
    );
    registry
        .get_mut(id)
        .expect("the port is live")
        .queue_output(b"queued-before-the-close");
    registry
        .close(&mut poller, id)
        .expect("the close drains the queue into the void-returning driver and finishes");
    assert!(
        !registry.contains(id),
        "a loaded driver's void output empties the queue inside the close, so nothing is left to park"
    );
    assert!(
        registry.is_closing(id).is_err(),
        "a finished port is no longer a live port, so the closing probe is a badarg"
    );
    let outcome = registry
        .execute_port_tasks(&mut poller, 1000)
        .expect("the round runs");
    assert_eq!(
        closed_of(&outcome, id),
        1,
        "the port's close is reported in the round that follows it"
    );
    let lines = log_lines(&log);
    assert_eq!(
        lines,
        vec![
            "start".to_string(),
            "flush".to_string(),
            "output".to_string(),
            "stop".to_string()
        ],
        "the driver saw the flush, the one write and the stop; log = {lines:?}"
    );
}
// ---------------------------------------------------------------------------------------------
// The data path: what `flush` does from inside its callback reaches the port
// ---------------------------------------------------------------------------------------------
/// The bytes a loaded driver's `flush` emits from **inside** its callback reach the port owner on the
/// ordinary sink path, and the close-time drain's echo follows them.
///
/// This is the leg that proves `DriverPort::flush` routes the entry through `DriverPort::call`: that
/// helper is what installs the [`crate::DriverContext`] (all four sinks) and publishes the current
/// `AbiHost`, so `driver_output` from `flush` becomes one read message — the
/// `PortNotification::Data` a runtime renders as `{Port, {data, _}}` — exactly as it would from
/// `output` or `timeout`. The log pins the order (`flush`, then the drain's `output`, then `stop`) and
/// the port's input holds `"F"` (the flush marker) followed by `"ab"` (the drain's echo).
///
/// Gated on [`host_symbols_exported`]: the fixture resolves `driver_output` out of the loading
/// process's dynamic symbol table, and the library's own unit-test binary does not export it (only the
/// integration test targets get `build.rs`'s `-Wl,--export-dynamic`). Where it is absent the test
/// prints the precise reason and returns — the ordering facts are still pinned by the base builds.
#[test]
fn the_marker_a_flush_emitting_from_inside_the_callback_reaches_the_port() {
    if !host_symbols_exported() {
        skip(
            "abi::flush: this binary does not export the driver_* host symbols, so a fixture that \
             calls driver_output cannot dlopen (only the integration test targets get \
             -Wl,--export-dynamic); the flush ordering itself is pinned by the host-call-free legs",
        );
        return;
    }
    let Some(so) = compile_fixture(&marker_fixture_source(), "flush_driver_marker") else {
        return;
    };
    let log = scratch_dir("flush-marker").join("callbacks.log");
    let (mut poller, mut registry, id) =
        open_fixture(&so, "flush_driver", &format!("log={}", log.display()));
    let port = registry.get_mut(id).expect("the port is live");
    port.queue_output(b"ab");
    port.close(&mut poller)
        .expect("the close flushes, drains the queued command and stops the driver");
    let lines = log_lines(&log);
    assert_eq!(
        lines,
        vec![
            "start".to_string(),
            "flush".to_string(),
            "output".to_string(),
            "stop".to_string()
        ],
        "flush, then the write, then stop; log = {lines:?}"
    );
    // Flush's `driver_output` then the drain's echo, both on the ordinary read sink.
    assert_eq!(
        port.drain_input(),
        b"Fab".to_vec(),
        "the flush marker then the bytes the drain handed the driver"
    );
    assert!(port.is_closed(), "the port finished its close");
}