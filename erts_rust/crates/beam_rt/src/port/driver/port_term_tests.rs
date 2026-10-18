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

//! The A.4 **`ERL_DRV_PORT` term-data operand regression suite** for
//! [`DriverHost::port_term`](beam_abi::DriverHost::port_term) on [`DriverPort`].
//!
//! # What the hook is for
//!
//! The C idiom for naming a port inside a driver-emitted term is `{ERL_DRV_PORT,
//! driver_mk_port(port)}` (`erl_driver.h:523`), and its operand is the port's **id term** — the C
//! decoder checks it with `is_internal_port` (`io.c:5744-5747`). This crate's `driver_mk_port`
//! answers the **handle** encoding instead (a port handle here *is* the [`DriverEnv`] address,
//! [`DriverEnv::as_port`]) because the term-data layer has no port table, so
//! [`decode_term_spec`](beam_abi::driver::host::decode_term_spec) resolves a non-port operand back
//! through the additive, defaulted [`DriverHost::port_term`](beam_abi::DriverHost::port_term) hook.
//! [`DriverPort`] answers it from the [`PortTerms`](super::PortTerms) handle
//! [`DriverPortAdapter::start`](super::DriverPortAdapter) stamps — the environment's address and the
//! port's [`PortId`], the two facts the host cannot observe about itself.
//!
//! # What is pinned, and at which level
//!
//! * **the seam, on a real environment** — a host whose environment was stamped resolves that
//!   *exact* handle to its own internal port term through the real decoder, and a foreign handle, a
//!   null handle and an unstamped host resolve to nothing (the decoder then refuses the whole spec,
//!   `ERTS_DDT_FAIL`, never a wrong term);
//! * **the gone bit** — after [`DriverPort::clear_driver_state`] the same handle answers nothing, so
//!   a driver that kept its `ErlDrvPort` cannot resurrect a dead port's id term;
//! * **end to end through a real C driver** — a `.so` whose `output` callback emits
//!   `{ERL_DRV_PORT, driver_mk_port(port)}` *twice over* (once as the `erl_drv_output_term` port
//!   argument, once as the spec operand), opened through the production `PortRegistry::open` path.
//!   `erl_drv_output_term` must answer `0` (accepted) and the delivered term must be the very port
//!   the registry opened — the assertion that fails with `-1` and no delivery when the hook is left
//!   at its default.
//!
//! # How the fixture reaches the host (`erl_drv_output_term`/`driver_mk_port`)
//!
//! A driver `.so` normally leaves `driver_*`/`erl_drv_*` **undefined** and resolves them out of the
//! loading process's dynamic symbol table, which exists because `build.rs` passes
//! `-Wl,--export-dynamic` **for the `[[bin]]` and `tests/` targets**. Cargo's `rustc-link-arg-tests`
//! does not cover a library's unit-test link, so this `--lib` harness has no `.dynsym` entry for
//! them and such a fixture would fail to `dlopen`. The fixture therefore imports **no** `driver_*`
//! symbol and is handed the two host functions it needs as `%lx` addresses in the loader's
//! `command` string (the `ready_output_tests.rs` convention), which it calls through.
//!
//! # Skip policy
//!
//! The end-to-end case needs the host C compiler and the host `erl_driver.h`; without either it
//! prints a **loud** `SKIP:` banner and returns — never a silent pass. The seam cases need neither.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_abi::driver::host::decode_term_spec;
use beam_abi::driver::types::{ERL_DRV_NIL, ERL_DRV_PORT, ERL_DRV_TUPLE};
use beam_alloc::Heap;
use beam_io::registry::PortRegistry;
use beam_io::{Poller, PortCommandOptions};
use beam_process::{PortId, ProcessId};
use beam_term::prelude::is_internal_port;
use beam_term::{AtomTable, NIL};

use super::{DriverPort, DriverPortAdapter, PortTerms, TermQueue};

/// The fixture: a driver whose `output` callback emits the **port term itself**, the C idiom
/// `{ERL_DRV_PORT, driver_mk_port(port)}`, and records what `erl_drv_output_term` answered.
///
/// Both host functions arrive as `%lx` addresses in the `command` string (see the module docs).
/// `output` is what `port_command/2` reaches; the marker line `R:<rc>` is the C return code, so a
/// refused spec (`-1`) is distinguishable from an accepted one (`0`) without reading the term.
const PORT_TERM_C: &str = r#"
#include <erl_driver.h>

#include <stdint.h>
#include <stdio.h>

/* The host's `erl_drv_output_term` and `driver_mk_port`, handed over by the test in the `command`
 * string (two `%lx` addresses). See the module docs: a `--lib` unit-test link does not export the
 * dynamic symbol table, so this fixture imports no `driver_*`/`erl_drv_*` symbol. */
typedef int (*rt_output_term_fn)(ErlDrvTermData, ErlDrvTermData *, int);
typedef ErlDrvTermData (*rt_mk_port_fn)(ErlDrvPort);

#define RT_MARKER_MAX 512

typedef struct {
    ErlDrvPort port;
    rt_output_term_fn output_term;
    rt_mk_port_fn mk_port;
    FILE *marker;
} rt_state;

/* One port per loaded object, so the state can live in this object's own storage: the fixture
 * deliberately does not import `driver_alloc`/`driver_free` either. */
static rt_state rt_state_storage;

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    rt_state *st = &rt_state_storage;
    unsigned long output_term_addr = 0;
    unsigned long mk_port_addr = 0;
    char marker[RT_MARKER_MAX];
    if (command == NULL)
        return (ErlDrvData) -1;
    if (sscanf(command, "%lx %lx %511s", &output_term_addr, &mk_port_addr, marker) != 3)
        return (ErlDrvData) -1;
    if (output_term_addr == 0 || mk_port_addr == 0)
        return (ErlDrvData) -1;
    st->output_term = (rt_output_term_fn) (intptr_t) output_term_addr;
    st->mk_port = (rt_mk_port_fn) (intptr_t) mk_port_addr;
    st->marker = fopen(marker, "w");
    if (st->marker == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void rt_stop(ErlDrvData drv_data) {
    rt_state *st = (rt_state *) drv_data;
    if (st != NULL && st->marker != NULL) {
        fclose(st->marker);
        st->marker = NULL;
    }
}

/* `port_command/2` reaches this callback. It emits a two-word spec whose only operand is the port
 * handle, exactly as the C doc example does, and records the host's answer. */
static void rt_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    rt_state *st = (rt_state *) drv_data;
    ErlDrvTermData spec[2];
    int rc;
    (void) buf;
    (void) len;
    if (st == NULL || st->output_term == NULL || st->mk_port == NULL)
        return;
    spec[0] = ERL_DRV_PORT;
    spec[1] = st->mk_port(st->port);
    rc = st->output_term(st->mk_port(st->port), spec, 2);
    if (st->marker != NULL) {
        fprintf(st->marker, "R:%d H:%lx\n", rc, (unsigned long) spec[1]);
        fflush(st->marker);
    }
}

static ErlDrvEntry rt_port_term_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = rt_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_PORT_TERM_NAME,
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

DRIVER_INIT(rt_port_term_init) {
    return &rt_port_term_entry;
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
/// A compact duplicate of `ready_output_tests.rs::erl_include_dir` (a unit test cannot import the
/// `tests/` crate target's support). Resolution order: ask the host `erl`; then the `include`
/// sibling of the `erts-*` directory the `erl` on `$PATH` lives in; then the standard roots.
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

/// A compiled driver `.so` plus the marker file its `output` callback writes.
struct Fixture {
    /// The scratch directory holding the source, the `.so` and the marker.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the `DriverPortAdapter::load` candidate).
    stem: PathBuf,
    /// The path the fixture records `erl_drv_output_term`'s answer in.
    marker: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output and the marker are not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The `erl_drv_output_term` answers the fixture recorded, each paired with the handle
    /// `driver_mk_port` handed it (`R:<rc> H:<handle>` in the marker, one line per call).
    fn answers(&self) -> Vec<(String, String)> {
        let text = match std::fs::read_to_string(&self.marker) {
            Ok(text) => text,
            // A missing marker means the fixture never got as far as `start`: report no answer so
            // the caller's assertion, not a panic, is what fails.
            Err(_) => return Vec::new(),
        };
        text.lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("R:")?;
                let (rc, handle) = rest.split_once(" H:")?;
                Some((rc.trim().to_string(), handle.trim().to_string()))
            })
            .collect()
    }
}

/// Compiles [`PORT_TERM_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing (the
/// caller returns early — **loudly**).
fn build_fixture(stem: &str) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!(
            "port_term: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("port_term: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_port_term_{stem}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{stem}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, PORT_TERM_C));
    if let Err(error) = write {
        skip(&format!(
            "port_term: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    // `cc -shared -fPIC -DRT_PORT_TERM_NAME="<stem>" -I<include> -o <dir>/<stem>.so <dir>/<stem>.c`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_PORT_TERM_NAME=\"{stem}\""))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "port_term: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("port_term: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !dir.join(format!("{stem}.so")).is_file() {
        skip(&format!("port_term: the C compiler produced no {stem}.so"));
        return None;
    }
    Some(Fixture {
        stem: dir.join(stem),
        marker: dir.join("marker.txt"),
        dir,
    })
}

// ---------------------------------------------------------------------------------------------
// The seam, on a real environment
// ---------------------------------------------------------------------------------------------

/// A host over a fresh atom table, running as pid 1, with a small arena — the shape the
/// `../driver.rs` unit suite and `hooks_tests.rs` build.
fn host() -> DriverPort {
    DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    )
}

/// The port term a stamped host answers for **its own** environment handle, and nothing for any
/// other operand.
///
/// The handle is taken from the real environment ([`DriverPort::environment`]) and the id from a
/// [`PortId`] the test picks, which is exactly the pair `DriverPortAdapter::start` stamps. The
/// decoder is the production one, so this pins the whole `ERL_DRV_PORT` arm: an internal port term
/// is pushed as-is, a non-port operand goes to the hook, and a hook answer that is not an internal
/// port refuses the spec.
#[test]
fn the_host_resolves_its_own_environment_handle_to_its_port_term() {
    let mut port_host = host();
    // Seized before the environment exists, exactly as `DriverPortAdapter::start` does: the
    // environment is what hands the handle out, and the host is never told it.
    let terms = port_host.port_terms();
    let mut env = port_host.environment();
    let handle = env.as_port() as usize;
    terms.set_env(handle);
    let port = PortId::new(7);
    terms.set_port(port);

    let expected = port.to_eterm();
    assert!(
        is_internal_port(expected),
        "the hook must answer an internal port term"
    );

    let term = decode_term_spec(&mut env, &[ERL_DRV_PORT, handle as u64])
        .expect("a stamped handle resolves through the hook");
    assert_eq!(
        term, expected,
        "the resolved term is this port's own id term"
    );
    assert!(is_internal_port(term), "and it really is a port");
    assert_eq!(
        PortId::from_eterm(term).expect("an internal port decodes"),
        port,
        "the term names the very port the lifecycle stamped"
    );

    // A handle that is not this port's environment, a null handle, and a plain non-port word are
    // all refused — the C's `NIL`, never a fabricated port term.
    assert_eq!(
        decode_term_spec(&mut env, &[ERL_DRV_PORT, 0x1000]),
        None,
        "a foreign handle is not this port"
    );
    assert_eq!(
        decode_term_spec(&mut env, &[ERL_DRV_PORT, 0]),
        None,
        "a null handle resolves to nothing"
    );
    assert_eq!(
        decode_term_spec(&mut env, &[ERL_DRV_PORT, 1]),
        None,
        "a low-nibble word that is not a port still goes through the hook"
    );

    // The hook's answer is load-bearing for compounds too: the same handle inside a spec builds a
    // term, and a refused handle aborts the whole spec (no partial delivery). The compound's own
    // shape is `decode_term_spec`'s business — this suite pins only that the operand resolved.
    let compound = decode_term_spec(
        &mut env,
        &[ERL_DRV_PORT, handle as u64, ERL_DRV_NIL, ERL_DRV_TUPLE, 2],
    )
    .expect("a tuple of [port, []] builds");
    assert!(
        compound != NIL,
        "the compound built from the resolved port term is a real term"
    );
    assert_eq!(
        decode_term_spec(
            &mut env,
            &[ERL_DRV_PORT, 0x1000, ERL_DRV_NIL, ERL_DRV_TUPLE, 2]
        ),
        None,
        "an unresolvable operand refuses the whole spec"
    );
}

/// A host nobody stamped answers nothing: the default state of a bare [`DriverPort`], and the only
/// safe answer while no port id exists to name.
#[test]
fn an_unstamped_host_answers_no_port_term() {
    let mut port_host = host();
    let terms = port_host.port_terms();
    let mut env = port_host.environment();
    let handle = env.as_port() as usize;

    assert_eq!(
        decode_term_spec(&mut env, &[ERL_DRV_PORT, handle as u64]),
        None,
        "an unpublished handle is not a port"
    );

    // A stamped handle with **no** port id is still nothing: the lifecycle stamps the two together
    // (`DriverPortAdapter::start`), and half a pair must not fabricate the other half.
    terms.set_env(handle);
    assert_eq!(decode_term_spec(&mut env, &[ERL_DRV_PORT, handle as u64]), None);
}

/// A port that has gone away answers nothing, even for the handle it published — the C's handle is
/// `NIL` once its port struct is gone (`erts_drvport2port`, `io.c:7295-7303`).
#[test]
fn a_gone_port_answers_no_port_term() {
    let mut port_host = host();
    let terms = port_host.port_terms();
    let mut env = port_host.environment();
    let handle = env.as_port() as usize;
    terms.set_env(handle);
    terms.set_port(PortId::new(3));

    assert_eq!(
        decode_term_spec(&mut env, &[ERL_DRV_PORT, handle as u64]),
        Some(PortId::new(3).to_eterm()),
        "while the port is live its handle resolves"
    );

    // `clear_driver_state` is the runtime's one "this port has left" signal — it runs from
    // `DriverPortAdapter::stop` and from its `Drop` — and the handle is shared by `Rc`, so this is
    // the very state the hook reads.
    port_host.clear_driver_state();
    assert_eq!(
        decode_term_spec(&mut env, &[ERL_DRV_PORT, handle as u64]),
        None,
        "a closed port's handle must not resolve to its old id term"
    );
}

// ---------------------------------------------------------------------------------------------
// End to end through a real C driver
// ---------------------------------------------------------------------------------------------

/// A loaded fixture driver plus the port's term queue, the poller and the registry it lives in.
struct Open {
    /// The compiled driver and its marker file.
    fixture: Fixture,
    /// The port's delivery queue for driver-emitted terms, seized from the host before the adapter
    /// consumed it (the boxed driver exposes no host) — the same handle the runtime's port pump
    /// drains.
    terms: TermQueue,
    /// The `ERL_DRV_PORT` resolution state, seized from the host for the same reason: the stamp
    /// `start` writes lands on this very handle (it is shared by `Rc`), so the suite can ask the
    /// hook's own question after the port is gone.
    port_terms: Rc<PortTerms>,
    /// The poller the port is registered with.
    poller: Poller,
    /// The registry the port lives in.
    ports: PortRegistry,
    /// The port's id.
    id: PortId,
}

/// Compiles the fixture, loads it and opens one real port on it.
///
/// The fixture is handed this runtime's `erl_drv_output_term`/`driver_mk_port` as two `%lx`
/// addresses in the `command` string (see the module docs).
fn open_port(stem: &str) -> Option<Open> {
    let fixture = build_fixture(stem)?;
    // The same anchor `crate::port::PortRegistry::new` uses, so the suite still exercises the
    // populated `erl_drv_*` table.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let port_host = DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    );
    // Seized before the host is consumed by `load`, and shared with the boxed host by `Rc`, so this
    // is the very queue the driver's `erl_drv_output_term` pushes onto and the very state its
    // `port_term` hook reads.
    let terms = port_host.term_queue();
    let port_terms = port_host.port_terms();
    let command = format!(
        "{:x} {:x} {}",
        beam_abi::driver::drv::erl_drv_output_term as *const () as usize,
        beam_abi::driver::drv::driver_mk_port as *const () as usize,
        fixture.marker.display()
    );
    let adapter = DriverPortAdapter::load(
        &[fixture.stem.display().to_string()],
        stem,
        port_host,
        command,
    )
    .expect("the fixture driver loads");
    let mut poller = Poller::new().expect("a poller");
    let mut ports = PortRegistry::new();
    let id = ports
        .open(&mut poller, Box::new(adapter))
        .expect("the port opens (the fixture's `start` recorded its marker file)");
    Some(Open {
        fixture,
        terms,
        port_terms,
        poller,
        ports,
        id,
    })
}

/// The **discriminating** end-to-end case: a real C driver's `{ERL_DRV_PORT, driver_mk_port(port)}`
/// operand delivers the port's own id term to the owner.
///
/// `port_command/2` reaches the fixture's `output`, which emits a two-word spec whose operand is the
/// handle `driver_mk_port` handed back, and records `erl_drv_output_term`'s answer. Delivered means
/// `0` *and* exactly one queued term that is the very [`PortId`] the registry opened.
///
/// Discriminates: with `DriverHost::port_term` left at its default the decoded operand is not an
/// internal port, so the whole spec is refused — the callback answers `-1`, nothing reaches the
/// queue, and every assertion below fails.
#[test]
fn a_c_drivers_erl_drv_port_operand_reaches_the_owner_as_the_port_term() {
    let Some(mut open) = open_port("rt_port_term") else {
        return;
    };
    let expected = open.id.to_eterm();
    assert!(
        is_internal_port(expected),
        "the opened port id encodes as an internal port"
    );

    open.ports
        .port_command(&mut open.poller, open.id, b"x", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    open.ports
        .execute_port_tasks(&mut open.poller, 1024)
        .expect("the port-task round runs");

    let answers = open.fixture.answers();
    assert_eq!(
        answers.len(),
        1,
        "the driver's `output` ran exactly once: {answers:?}"
    );
    let (answer, handle) = &answers[0];
    assert_eq!(
        answer, "0",
        "`erl_drv_output_term` accepted the spec — a refused one answers -1"
    );

    // The handle the C got from `driver_mk_port` resolves, through the very same shared state the
    // hook reads, to this port's term — and never by dereferencing it.
    let handle = usize::from_str_radix(handle, 16).expect("the fixture printed a hex handle");
    assert_eq!(
        open.port_terms.resolve(handle),
        Some(expected),
        "the published handle resolves to the port the runtime opened"
    );

    let queued = open.terms.take();
    assert_eq!(
        queued.len(),
        1,
        "the driver delivered exactly one term: {queued:?}"
    );
    let (destination, term) = queued[0];
    assert_eq!(
        destination, None,
        "`erl_drv_output_term` targets the port owner, not a specific process"
    );
    assert!(
        is_internal_port(term),
        "the delivered term is the port's internal port term"
    );
    assert_eq!(
        term, expected,
        "and it names the very port the runtime opened"
    );

    let Open {
        fixture,
        mut poller,
        mut ports,
        id,
        ..
    } = open;
    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// A port that has gone away stops answering — end to end, on the handle a real C driver published.
///
/// The live half is [`a_c_drivers_erl_drv_port_operand_reaches_the_owner_as_the_port_term`]'s
/// assertion; this case closes the port (which runs the driver's `stop`, hence
/// [`DriverPort::clear_driver_state`]) and asks the same question of the same handle. It must answer
/// nothing: the C's handle is `NIL` once its port struct is gone (`erts_drvport2port`,
/// `io.c:7295-7303`), and a driver that kept its `ErlDrvPort` in its private state must not be able
/// to turn it back into a dead port's id term.
#[test]
fn a_closed_ports_published_handle_no_longer_resolves() {
    let Some(mut open) = open_port("rt_port_term_gone") else {
        return;
    };
    let expected = open.id.to_eterm();
    open.ports
        .port_command(&mut open.poller, open.id, b"x", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    open.ports
        .execute_port_tasks(&mut open.poller, 1024)
        .expect("the port-task round runs");
    let answers = open.fixture.answers();
    assert_eq!(answers.len(), 1, "the driver's `output` ran once");
    let handle = usize::from_str_radix(&answers[0].1, 16).expect("a hex handle");
    assert_eq!(
        open.port_terms.resolve(handle),
        Some(expected),
        "while the port is live its published handle resolves"
    );

    let Open {
        fixture,
        terms,
        port_terms,
        mut poller,
        mut ports,
        id,
    } = open;
    ports.close(&mut poller, id).expect("the port closes");
    assert_eq!(
        port_terms.resolve(handle),
        None,
        "after the port left the runtime the same handle resolves to nothing"
    );
    // `close` ran the driver's `stop` and emptied the term queue with it: a term built on the host
    // arena must not outlive it.
    assert!(
        terms.take().is_empty(),
        "a closed port delivers none of the terms it queued"
    );
    fixture.cleanup();
}
