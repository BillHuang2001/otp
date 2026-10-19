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

//! The A.4 **async-job pool** end-to-end suite: the C's `driver_async` (`erl_async.c`) and the
//! emulator's `+A N`, driven through the real port round.
//!
//! # What is pinned, and where
//!
//! The pool itself is `beam_abi`'s ([`beam_abi::driver::AsyncPool`], `erl_async.c`); the delivery
//! half — the per-port ready queue, the `+A 0` inline hand-offs, the outstanding count and the
//! deferred unloads — is [`AsyncReady`](super::async_ready::AsyncReady); the completion policy is
//! [`DriverPortAdapter::deliver_pooled_async`](super::driver::DriverPortAdapter) (pooled jobs) and
//! [`DriverHost::deliver_ready_async`](beam_abi::DriverHost::deliver_ready_async) (the inline path);
//! and the drain cadence is [`PortRegistry::pump`](super::PortRegistry)'s
//! ([`PortRegistry::drain_async_pool`](super::PortRegistry)).
//!
//! Every case here runs a **real** port opened through the **production**
//! `open_port({spawn_driver, "<object> <log>"}, [])` path ([`beam_bifs::PortHook::open_port`]), so
//! the port's host really does share [`PortRegistry::async_ready`](super::PortRegistry) and the
//! round really is [`PortRegistry::pump`](super::PortRegistry). The observable is a C fixture
//! compiled at test time: `control` submits a job and answers the acceptance as **one byte**,
//! `async_invoke` records that it ran, `ready_async` records the `async_data` it was handed and
//! writes one distinguishable byte back to the owner through `driver_output`, and `async_free`
//! records its own run.
//!
//! * **(a) a positive job id** — the reply byte decodes to **49** (`'1'`, id `> 0`),
//!   `async_invoke` ran, `ready_async` received the **same** `async_data`, `async_free` did not
//!   run, and the owner received the `'R'` byte the callback wrote;
//! * **(b) the port-exit path** — a job submitted and the port then closed ⇒ `async_free` runs
//!   **exactly once** and no `ready_async` is ever called (the C's `!p` branch,
//!   `erl_async.c:320-330`);
//! * **(c) the `+A 0` inline path** (`AsyncPool::new(0)`) — `driver_async` answers **0**, the job
//!   runs on the submitting thread, and the completion policy is the same one (no `async_free`
//!   when `ready_async` took the data);
//! * **no `ready_async` slot** — the C's `need_free = 1` (`io.c:5400`): `async_free` runs exactly
//!   once and nothing is delivered, on **both** the inline and the pooled path.
//!
//! # Measured oracle byte table
//!
//! The same `.so` source (this file's [`ASYNC_C`]) was built against the host `erl_driver.h` and
//! driven under the system `erl` (OTP 29.1.1 / erts-17.1) with
//! `erl_ddll:load_driver/2`, `open_port({spawn_driver, Name}, [])`, `port_control/3`, and every
//! probe terminated with `-s init stop`:
//!
//! | probe | `port_control(P, 1, <<>>)` | owner's `ready_async` byte | fixture log |
//! |---|---|---|---|
//! | `erl +A 2` | `[49]` (`'1'`, id `> 0`) | `<<82>>` (`'R'`) | `submit 1 1 …` / `invoke …` / `ready …` |
//! | `erl +A 0` | `[49]` | `<<82>>` | `submit 1 0 …` / `invoke …` / `ready …` |
//! | `erl +A 2`, submit then `port_close/1` | `[49]` | — (no `ready_async`) | `invoke …` / `free …` (exactly one `free`) |
//!
//! The oracle's `+A 0` row is **49, not 0**: the CLI clamps `+A 0` to one async thread
//! (`erts/emulator/beam/erl_init.c`: `if (erts_async_max_threads < 1) erts_async_max_threads =
//! 1;`), so the C's inline `+A 0` branch (`erl_async.c:603-616`, `return 0`) is not reachable from
//! the command line. `AsyncPool::new(0)` is that branch's own configuration — a genuinely
//! zero-worker pool — and its documented answer is the C's `0`, which is what case (c) pins.
//!
//! The **pre-fix** value for the default (`+A 1`) path was the reply byte **48** (`'0'`, i.e.
//! `driver_async` answering `-1` down the frozen seam's legacy hook): `driver_async` never reached
//! a pool because no host installed one. The ledger entry is
//! `erts_rust/tools/differential/divergences.md` § A.4. Case (c)'s `48` is a *different* fact — the
//! inline configuration's honest `0` — and is asserted as such.
//!
//! # How the fixture reaches what it needs
//!
//! A `--lib` unit-test link is not given `-Wl,--export-dynamic` (only the `bins`/`tests` targets
//! are, by `build.rs`), so the fixture cannot import `driver_async`/`driver_output` as ordinary
//! dynamic symbols. It resolves both with `dlsym(RTLD_DEFAULT, …)` **first** — the form the oracle
//! needs, whose `beam.smp` exports both — and otherwise takes the two addresses the test hands it
//! in the loader `command` string (`%lx` of [`beam_abi::driver::drv::driver_api_table`]'s
//! `driver_async` and `driver_output` slots). One `.so` source therefore serves both runtimes,
//! which is what makes the measured table above the *same* fixture this suite runs.
//!
//! # Skip policy
//!
//! Needs the host C compiler and the host `erl_driver.h`; without either it prints a **loud**
//! `SKIP:` banner and returns — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::PortHook;
use beam_process::{PortId, ProcessId};
use beam_term::{make_atom, AtomTable, NIL};

use crate::config::RuntimeConfig;
use crate::port::PortEvent;
use crate::Runtime;

/// The fixture driver.
///
/// `RT_ASYNC_NAME` is the entry's `driver_name`; `RT_ASYNC_NO_READY` builds the variant whose
/// `ErlDrvEntry::ready_async` slot is `NULL` (the C's `need_free = 1`, `io.c:5400`).
///
/// The fixture keeps its state in its own storage (one port per loaded object) so it imports
/// neither `driver_alloc`/`driver_free` nor any symbol beyond the two it resolves itself. Every
/// observable is a line in the log file the loader `command` names — `submit <id> <key> <data>`,
/// `invoke <data>`, `ready <data>`, `free <data>` — plus the one byte `ready_async` writes to the
/// owner through `driver_output`.
const ASYNC_C: &str = r#"
#include <erl_driver.h>

#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#define RT_TOKEN_MAX 512

typedef long (*rt_async_fn)(ErlDrvPort, unsigned int *, void (*)(void *), void *,
                            void (*)(void *));
typedef int (*rt_output_fn)(ErlDrvPort, char *, ErlDrvSizeT);

typedef struct {
    int log_fd;
    ErlDrvPort port;
    void *data;
} rt_state;

static rt_state rt_storage;
static rt_async_fn rt_async;
static rt_output_fn rt_output;

static void rt_note(rt_state *st, const char *what, void *data) {
    if (st != NULL && st->log_fd >= 0)
        dprintf(st->log_fd, "%s %lx\n", what, (unsigned long) data);
}

/* Runs on an `erts_async_N` worker thread (`erl_async.c:416-442`). It only records that it ran:
 * the job's data is never dereferenced here (nor anywhere else in the fixture). */
static void rt_async_invoke(void *data) {
    rt_note(&rt_storage, "invoke", data);
    /* Long enough that the port-exit case closes while the job is still in flight. */
    usleep(20000);
}

/* The C's `if (async_free) async_free(async_data)`: exactly once, whichever path asks for it. */
static void rt_async_free(void *data) {
    rt_note(&rt_storage, "free", data);
}

#ifndef RT_ASYNC_NO_READY
/* The callback under test: it is handed the driver's state and the submitted `async_data`, and it
 * answers by writing one distinguishable byte back to the port owner. */
static void rt_ready_async(ErlDrvData drv_data, ErlDrvThreadData data) {
    rt_state *st = (rt_state *) drv_data;
    char reply = 'R';
    rt_note(st, "ready", (void *) data);
    if (st != NULL && rt_output != NULL && st->port != NULL)
        rt_output(st->port, &reply, 1);
}
#endif

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    rt_state *st = &rt_storage;
    char object[RT_TOKEN_MAX];
    char log[RT_TOKEN_MAX];
    unsigned long submit = 0;
    unsigned long emit = 0;
    int fields;

    object[0] = '\0';
    log[0] = '\0';
    if (command == NULL)
        return (ErlDrvData) -1;
    /* `<driver object> <log path> [<driver_async> <driver_output>]`: the production `Name` keeps the
     * object first (that is the prefix the loader resolves), the log second, and the two host
     * addresses last when the test had to hand them over. */
    fields = sscanf(command, "%511s %511s %lx %lx", object, log, &submit, &emit);
    if (fields < 2)
        return (ErlDrvData) -1;
    st->log_fd = open(log, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (st->log_fd < 0)
        return (ErlDrvData) -1;
    st->port = port;
    /* One job record per loaded object: a stable pointer, which is also what makes the identity of
     * the `async_data` `ready_async` receives checkable. */
    st->data = (void *) &rt_storage;
    /* The two host entries, resolved the way each runtime can: `dlsym` under the oracle (whose
     * `beam.smp` exports both), the handed-over addresses otherwise. */
    rt_async = (rt_async_fn) dlsym(RTLD_DEFAULT, "driver_async");
    rt_output = (rt_output_fn) dlsym(RTLD_DEFAULT, "driver_output");
    if (rt_async == NULL && submit != 0)
        rt_async = (rt_async_fn) submit;
    if (rt_output == NULL && emit != 0)
        rt_output = (rt_output_fn) emit;
    if (rt_async == NULL)
        return (ErlDrvData) -1;
    return (ErlDrvData) st;
}

static void rt_stop(ErlDrvData drv_data) {
    rt_state *st = (rt_state *) drv_data;
    if (st != NULL && st->log_fd >= 0) {
        close(st->log_fd);
        st->log_fd = -1;
    }
}

/* Verb 1 submits one job and answers the acceptance as ONE byte: '1' when `driver_async` returned a
 * positive id, '0' when it answered 0 (the `+A 0` inline path). The reply is written into the
 * caller's own buffer (`*rbuf`), which is the C's `char resp_buf[ERL_ONHEAP_BINARY_LIMIT]`. */
static int rt_control(ErlDrvPort port, unsigned int command, char *buf, ErlDrvSizeT len,
                      char **rbuf, ErlDrvSizeT rlen) {
    rt_state *st = &rt_storage;
    unsigned int key = 0;
    long id;
    char *reply;
    (void) buf;
    (void) len;
    if (command != 1 || rt_async == NULL)
        return -1;
    if (rlen < 1 || rbuf == NULL || *rbuf == NULL)
        return -1;
    id = rt_async(port, &key, rt_async_invoke, st->data, rt_async_free);
    if (st->log_fd >= 0)
        dprintf(st->log_fd, "submit %ld %u %lx\n", id, key, (unsigned long) st->data);
    reply = *rbuf;
    reply[0] = (id > 0) ? '1' : '0';
    return 1;
}

static ErlDrvEntry rt_async_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_ASYNC_NAME,
    .finish = NULL,
    .handle = NULL,
    .control = rt_control,
    .timeout = NULL,
    .outputv = NULL,
#ifdef RT_ASYNC_NO_READY
    .ready_async = NULL,
#else
    .ready_async = rt_ready_async,
#endif
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

DRIVER_INIT(rt_async_init) {
    return &rt_async_entry;
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
/// A compact duplicate of `failure_e2e_tests.rs::erl_include_dir` (a unit test cannot import the
/// `tests/` crate target's support), extended with the `<root>/erts-<vsn>/include` layout a
/// release installation really uses. Resolution order: ask the host `erl`; then the `include`
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
///
/// `<root>/include` and `<root>/erts-*/include` (the release layout: `code:root_dir()` holds the
/// `erts-<vsn>` directory itself, not only a `lib/erlang` copy) are both tried, then the
/// `lib/erlang` variants the source tree uses.
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

/// The two host entry points the fixture needs, as addresses it can be handed in its `command`.
///
/// Taken from [`beam_abi::driver::drv::driver_api_table`] — the published `driver_*` table this
/// runtime's own `dlopen`ed drivers call through — because the `--lib` test link exports no dynamic
/// symbols (see the module docs). `None` when a slot is not populated, which the caller reports as
/// a skip.
fn host_addresses() -> Option<(usize, usize)> {
    let table = beam_abi::driver::drv::driver_api_table();
    let submit = table.driver_async? as usize;
    let emit = table.driver_output? as usize;
    Some((submit, emit))
}

/// A compiled driver `.so` plus the log file its callbacks write to.
struct Fixture {
    /// The scratch directory holding the source, the `.so` and the log.
    dir: PathBuf,
    /// The `.so` path **with** its suffix (the first token of the production `Name`, which is the
    /// driver object the loader resolves).
    object: PathBuf,
    /// The file the fixture records every callback in.
    log: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output and the log are not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// Every log line written so far.
    fn lines(&self) -> Vec<String> {
        match std::fs::read_to_string(&self.log) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            // A missing log means the fixture never got as far as `start`: no lines, so the
            // caller's assertion (not a panic) is what fails.
            Err(_) => Vec::new(),
        }
    }

    /// The log lines starting with `prefix` (e.g. `"free "`).
    fn matching(&self, prefix: &str) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|line| line.starts_with(prefix))
            .collect()
    }

    /// The single `async_data` identity every line records, split out of a `"<what> <hex>"` line.
    fn data_of(line: &str) -> &str {
        line.split(' ').nth(1).unwrap_or("")
    }

    /// The production `Name` **command** string this fixture is opened with.
    ///
    /// `<object> <log> <driver_async addr> <driver_output addr>`: the object first (the loader
    /// resolves the prefix up to the first blank), then the log path, then the two host entries the
    /// `--lib` link cannot publish.
    fn command(&self) -> Option<String> {
        let (submit, emit) = host_addresses()?;
        Some(format!(
            "{} {} {:x} {:x}",
            self.object.display(),
            self.log.display(),
            submit,
            emit
        ))
    }
}

/// Compiles [`ASYNC_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing (the
/// caller returns early — **loudly**).
///
/// `no_ready` selects the `RT_ASYNC_NO_READY` variant (an entry with **no** `ready_async` slot).
fn build_fixture(stem: &str, no_ready: bool) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!(
            "async jobs: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("async jobs: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_async_ready_{stem}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{stem}.c"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, ASYNC_C));
    if let Err(error) = write {
        skip(&format!(
            "async jobs: cannot prepare {}: {error}",
            dir.display()
        ));
        return None;
    }
    let object = dir.join(format!("{stem}.so"));
    // `cc -shared -fPIC -DRT_ASYNC_NAME="<stem>" [-DRT_ASYNC_NO_READY] -I<include> -o <so> <src>`
    // (`-ldl` for the `dlsym` resolution the oracle path needs).
    let mut command = Command::new(cc());
    command
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_ASYNC_NAME=\"{stem}\""));
    if no_ready {
        command.arg("-DRT_ASYNC_NO_READY");
    }
    let output = command
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(&object)
        .arg(&source)
        .arg("-ldl")
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "async jobs: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("async jobs: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !object.is_file() {
        skip(&format!("async jobs: the C compiler produced no {stem}.so"));
        return None;
    }
    Some(Fixture {
        log: dir.join("log.txt"),
        object,
        dir,
    })
}

// ---------------------------------------------------------------------------------------------
// Driving the production port path
// ---------------------------------------------------------------------------------------------

/// A fixture opened as one live runtime port.
struct Open {
    /// The compiled driver and its log.
    fixture: Fixture,
    /// The runtime the port lives in (its registry is the one the port round drains).
    runtime: Runtime,
    /// The port's id.
    id: PortId,
}

/// Opens `fixture` through the **production** `open_port({spawn_driver, …})` path with `threads`
/// async workers configured first (`0` = the inline configuration).
fn open(fixture: Fixture, threads: usize) -> Option<Open> {
    let runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    // Before the first driver port: `AsyncReady::set_threads` is the `+A N` stamp, and the pool
    // itself is built lazily on the first `driver_async`.
    runtime
        .ports
        .borrow_mut()
        .set_async_threads(threads);
    let command = fixture.command()?;
    let mut atoms = AtomTable::new();
    let tag = make_atom(atoms.intern("spawn_driver"));
    let atoms = Rc::new(atoms);
    let mut heap = Heap::new(512).expect("a test heap");
    let command = crate::port::charlist(&mut heap, &command).expect("a command string");
    let name = heap.tuple(&[tag, command]).expect("a name tuple");
    let owner = ProcessId::new(1, 0);
    let mut hook = crate::port::RuntimePorts::new(Rc::clone(&runtime.ports), atoms, owner);
    let id = hook
        .open_port(name, NIL)
        .expect("the fixture driver loads and registers through the production path");
    Some(Open {
        fixture,
        runtime,
        id,
    })
}

impl Open {
    /// Submits one job through the driver's `control` callback and answers its reply bytes.
    fn submit(&mut self) -> Vec<u8> {
        self.runtime
            .ports
            .borrow_mut()
            .port_control(self.id, "1", &[])
            .expect("the fixture's control callback answers")
    }

    /// One real port round — the runtime's own [`PortRegistry::pump`](super::PortRegistry).
    fn round(&mut self, timeout: Duration) -> Vec<PortEvent> {
        self.runtime
            .ports
            .borrow_mut()
            .pump(timeout)
            .expect("the port round runs")
    }

    /// Rounds until `port` produces a stream byte (bounded), returning every byte it produced in
    /// order. A closed port never produces one, so a caller that has closed the port expects the
    /// empty vector after its own bound elapses.
    fn wait_for_data(&mut self, bound: Duration) -> Vec<u8> {
        let deadline = Instant::now() + bound;
        let mut bytes = Vec::new();
        while Instant::now() < deadline {
            for event in self.round(Duration::from_millis(20)) {
                if let PortEvent::Data {
                    port: source,
                    read,
                    ..
                } = event
                {
                    if source == self.id {
                        bytes.extend(read.flattened());
                    }
                }
            }
            if !bytes.is_empty() {
                return bytes;
            }
        }
        bytes
    }

    /// Rounds until the fixture logs a `prefix` line (bounded), returning every such line.
    fn wait_for_line(&mut self, prefix: &str, bound: Duration) -> Vec<String> {
        let deadline = Instant::now() + bound;
        let mut found = self.fixture.matching(prefix);
        while found.is_empty() && Instant::now() < deadline {
            self.round(Duration::from_millis(20));
            found = self.fixture.matching(prefix);
        }
        found
    }

    /// Closes the port through the runtime's registry (the driver's `stop` and the
    /// [`AsyncReady`](super::async_ready::AsyncReady) detach ride that).
    fn close(&mut self) {
        self.runtime
            .ports
            .borrow_mut()
            .close(self.id)
            .expect("the port closes");
    }
}

/// The `driver_async` acceptance byte the oracle answers for a positive id (`'1'`, id `> 0`).
const ACCEPTED: u8 = b'1';

/// The acceptance byte the inline (`+A 0`) path answers — the C's `return 0`.
const INLINE: u8 = b'0';

/// The one byte the fixture's `ready_async` writes to the owner.
const READY_BYTE: u8 = b'R';

/// How long a case waits for a completion produced on another thread or by a later round.
const BOUND: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// (a) the positive job id
// ---------------------------------------------------------------------------------------------

/// **(a) POSITIVE ID.** `driver_async` answers a positive id (the reply byte is **49**, `'1'`), the
/// job's `async_invoke` really ran, `ready_async` received **exactly** the submitted `async_data`,
/// `async_free` never ran, and the byte the callback wrote reached the owner.
///
/// Discriminates: the pre-fix runtime answered **48** (`'0'`, `-1` — no pool was installed, so
/// `driver_async` fell back to the frozen seam's legacy hook) and never ran `async_invoke` at all.
#[test]
fn a_submitted_job_gets_a_positive_id_and_its_ready_async_receives_the_data() {
    let Some(fixture) = build_fixture("rt_async_once", false) else {
        return;
    };
    let Some(mut open) = open(fixture, 1) else {
        skip("async jobs: the driver_async/driver_output slots are not populated");
        return;
    };

    let reply = open.submit();
    assert_eq!(
        reply,
        vec![ACCEPTED],
        "the reply byte must decode to 49 ('1', the oracle's `[49]`): a positive job id"
    );

    let bytes = open.wait_for_data(BOUND);
    assert_eq!(
        bytes,
        vec![READY_BYTE],
        "the owner receives the one byte `ready_async` wrote through `driver_output`"
    );

    let submits = open.fixture.matching("submit ");
    let invokes = open.fixture.matching("invoke ");
    let readies = open.fixture.matching("ready ");
    assert_eq!(submits.len(), 1, "one submission: {submits:?}");
    assert_eq!(invokes.len(), 1, "the job ran on a worker: {invokes:?}");
    assert_eq!(readies.len(), 1, "one `ready_async` call: {readies:?}");
    let id: i64 = submits[0]
        .split(' ')
        .nth(1)
        .and_then(|field| field.parse().ok())
        .expect("the submittal id");
    assert!(id > 0, "the id the driver was handed is positive: {id}");
    assert_eq!(
        Fixture::data_of(&readies[0]),
        Fixture::data_of(&invokes[0]),
        "`ready_async` receives the very `async_data` `async_invoke` ran with"
    );
    assert_eq!(
        Fixture::data_of(&readies[0]),
        Fixture::data_of(&submits[0]),
        "...which is the one the submission carried"
    );
    assert!(
        open.fixture.matching("free ").is_empty(),
        "the driver's `ready_async` took the data, so `async_free` must NOT run: {:?}",
        open.fixture.matching("free ")
    );

    open.close();
    open.fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// (b) the port-exit path
// ---------------------------------------------------------------------------------------------

/// **(b) PORT EXIT.** A job is submitted and the port then closed: `async_free` runs **exactly
/// once** and `ready_async` is never called — the C's `!p` branch (`erl_async.c:320-330`).
///
/// Discriminates: without the detach/`port_closed` wiring the closed port's job would either be
/// delivered to a `dlclose`d driver or never released at all (a silent leak of the driver's
/// `async_data`).
#[test]
fn closing_the_port_releases_the_job_exactly_once_without_a_ready_async() {
    let Some(fixture) = build_fixture("rt_async_exit", false) else {
        return;
    };
    let Some(mut open) = open(fixture, 1) else {
        skip("async jobs: the driver_async/driver_output slots are not populated");
        return;
    };

    assert_eq!(open.submit(), vec![ACCEPTED], "the job was accepted");
    // Closed while the worker is still inside `async_invoke` (or with the completion already
    // queued) — either way the port is gone before the round that would deliver it.
    open.close();

    let frees = open.wait_for_line("free ", BOUND);
    assert_eq!(
        frees.len(),
        1,
        "the gone port's job is released exactly once: {frees:?}"
    );
    assert_eq!(
        Fixture::data_of(&frees[0]),
        Fixture::data_of(&open.fixture.matching("invoke ")[0]),
        "the released `async_data` is the one the job ran with"
    );
    assert!(
        open.fixture.matching("ready ").is_empty(),
        "a port that is gone gets no `ready_async`: {:?}",
        open.fixture.matching("ready ")
    );

    // The port is closed, so nothing of its bytes can be reported any more.
    let bytes = open.wait_for_data(Duration::from_millis(100));
    assert!(
        bytes.is_empty(),
        "a closed port reports no data: {bytes:?}"
    );

    open.fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// (c) the `+A 0` inline path
// ---------------------------------------------------------------------------------------------

/// **(c) `+A 0` INLINE.** With zero async workers (`AsyncPool::new(0)`) `driver_async` answers
/// **0**, the job runs on the submitting thread, and the completion policy is the same one: the
/// driver's `ready_async` takes the data, so `async_free` does not run.
///
/// The oracle's own `+A 0` probe answers 49, because the CLI clamps the flag to one thread
/// (`erl_init.c`); `set_async_threads(0)` is the C's *unclamped* inline configuration
/// (`erl_async.c:603-616`), whose answer is its documented `return 0`.
#[test]
fn the_inline_path_answers_zero_and_runs_the_job_on_the_submitting_thread() {
    let Some(fixture) = build_fixture("rt_async_inline", false) else {
        return;
    };
    let Some(mut open) = open(fixture, 0) else {
        skip("async jobs: the driver_async/driver_output slots are not populated");
        return;
    };

    let reply = open.submit();
    assert_eq!(
        reply,
        vec![INLINE],
        "`+A 0` answers the C's 0 (the inline path), never an id"
    );

    let bytes = open.wait_for_data(BOUND);
    assert_eq!(
        bytes,
        vec![READY_BYTE],
        "the inline job's `ready_async` still reaches the owner"
    );

    let submits = open.fixture.matching("submit ");
    let invokes = open.fixture.matching("invoke ");
    let readies = open.fixture.matching("ready ");
    assert_eq!(submits.len(), 1, "one submission: {submits:?}");
    assert_eq!(invokes.len(), 1, "the job ran inline: {invokes:?}");
    assert_eq!(readies.len(), 1, "one `ready_async` call: {readies:?}");
    assert_eq!(
        submits[0].split(' ').nth(1),
        Some("0"),
        "the id the driver was handed is 0: {submits:?}"
    );
    assert_eq!(
        Fixture::data_of(&readies[0]),
        Fixture::data_of(&invokes[0]),
        "the same `async_data`, on the same thread"
    );
    assert!(
        open.fixture.matching("free ").is_empty(),
        "the inline hand-off consumed the data, so no `async_free`: {:?}",
        open.fixture.matching("free ")
    );

    open.close();
    open.fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// The C's `need_free = 1`: an entry with no `ready_async` slot
// ---------------------------------------------------------------------------------------------

/// **No `ready_async` slot, pooled.** The entry leaves `ErlDrvEntry::ready_async` `NULL`, so the
/// completion frees the data (`io.c:5400`) instead of delivering it — `async_free` runs exactly
/// once, no byte reaches the owner, and the job still gets a positive id.
#[test]
fn an_entry_without_ready_async_frees_the_pooled_job_exactly_once() {
    let Some(fixture) = build_fixture("rt_async_noready", true) else {
        return;
    };
    let Some(mut open) = open(fixture, 1) else {
        skip("async jobs: the driver_async/driver_output slots are not populated");
        return;
    };

    assert_eq!(open.submit(), vec![ACCEPTED], "the job was accepted");
    let frees = open.wait_for_line("free ", BOUND);
    assert_eq!(
        frees.len(),
        1,
        "the entry has no `ready_async`, so the data is released exactly once: {frees:?}"
    );
    assert_eq!(open.fixture.matching("invoke ").len(), 1, "the job ran");
    assert!(
        open.fixture.matching("ready ").is_empty(),
        "there is no `ready_async` slot to call"
    );
    assert!(
        open.wait_for_data(Duration::from_millis(100)).is_empty(),
        "nothing is delivered to the owner"
    );

    open.close();
    open.fixture.cleanup();
}

/// **No `ready_async` slot, inline.** The same policy on the `+A 0` path: `driver_async` answers 0,
/// the job runs on the submitting thread, and its `async_free` runs exactly once.
#[test]
fn an_entry_without_ready_async_frees_the_inline_job_exactly_once() {
    let Some(fixture) = build_fixture("rt_async_noready_inline", true) else {
        return;
    };
    let Some(mut open) = open(fixture, 0) else {
        skip("async jobs: the driver_async/driver_output slots are not populated");
        return;
    };

    assert_eq!(open.submit(), vec![INLINE], "`+A 0` answers 0");
    let frees = open.fixture.matching("free ");
    assert_eq!(
        frees.len(),
        1,
        "the inline job is released exactly once, before `control` even returns: {frees:?}"
    );
    assert_eq!(open.fixture.matching("invoke ").len(), 1, "the job ran");
    assert!(
        open.fixture.matching("ready ").is_empty(),
        "there is no `ready_async` slot to call"
    );
    assert!(
        open.wait_for_data(Duration::from_millis(100)).is_empty(),
        "nothing is delivered to the owner"
    );

    open.close();
    open.fixture.cleanup();
}
