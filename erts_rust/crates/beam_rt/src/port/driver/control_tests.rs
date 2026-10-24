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

//! The A.4 **port-control regression suite** for
//! [`DriverPortAdapter::control`](super::DriverPortAdapter::control) — the driver half of
//! `erlang:port_control/3` — and for the runtime-side entry point that reaches it,
//! [`crate::port::PortRegistry::port_control`].
//!
//! The C ground truth is `call_driver_control` (`io.c:4024-4078`):
//!
//! * the entry's `control` slot is handed `(data, cmd, buf, len, *rbuf, rlen)`, where `rbuf`
//!   **initially** points at the caller's `char resp_buf[ERL_ONHEAP_BINARY_LIMIT]`
//!   (`io.c:4352-4354`) and the driver may re-point it at memory of its own;
//! * an absent `control` slot is `ERTS_PORT_OP_BADARG` (`io.c:4035`) and a negative `cres` is
//!   `ERTS_PORT_OP_BADARG` too (`io.c:4074`);
//! * `!resp_bufp` is the C's `NIL` (`write_port_control_result`, `io.c:4126`) — an empty reply,
//!   not an error.
//!
//! [`DriverPortAdapter::control`](super::DriverPortAdapter::control) reproduces all four, and its
//! one deliberate gap — a reply the driver answers from a buffer *it* allocated, which this
//! `#![deny(unsafe_code)]` crate cannot read — is a typed *unsupported*, never an empty or
//! truncated reply. This suite pins each of those outcomes.
//!
//! # What the fixture observes
//!
//! `control` receives the private state and the four buffers, so the fixture can report both the
//! reply and what it was *asked*: every call appends a `C:<cmd>:<len>` line to a **marker file**
//! whose path the test hands it in the loader's `command` string. That is what proves the verb
//! reached the driver as the **parsed integer** and the request bytes with their **real length** —
//! not a substituted command and not a truncated buffer.
//!
//! The `command` string therefore carries three fields — the object path, the address of the host's
//! `set_port_control_flags` entry point and the marker path — of which only the first is required
//! (see the fixture's `rt_start`). The address is there because a driver `.so` cannot *import*
//! that symbol from this binary: a `--lib` unit-test harness publishes no global dynamic symbol at
//! all, so `dlopen` would fail with `undefined symbol: set_port_control_flags`. The oracle probe in
//! [`control_reply_oracle_tests`](super::control_reply_oracle_tests) compiles the **same** source
//! and hands no address, because a real emulator does export the symbol (`nm -D beam.smp`).//!
//! The reply verbs are chosen so the reply cannot be confused with the request: `RT_CTL_REVERSE`
//! **reverses** the request (so a reply that happened to echo the caller's own buffer would fail),
//! `RT_CTL_FIXED` answers a constant, `RT_CTL_NIL` nulls the reply pointer (the C's `NIL`),
//! `RT_CTL_FOREIGN` points it at a buffer **inside the loaded object**, and `RT_CTL_NEGATIVE`
//! returns `-1`.
//!
//! # The reply-**shape** verbs (`26`..`30`, `beam_io`'s own numbering)
//!
//! `write_port_control_result` (`io.c:4126-4175`) renders a reply from **two** facts, tested in
//! this order: the driver's reply pointer (`if (!resp_bufp) return NIL;`, `io.c:4134`) and then the
//! port's `PORT_CONTROL_FLAG_BINARY` word (`io.c:4136-4162` binary / `io.c:4163-4166` int list).
//! Two of the four rows therefore produce **zero bytes** and are told apart by nothing but the
//! shape, so the fixture drives them with the same opcode numbers `beam_io`'s own C-driver fixture
//! uses (`crates/beam_io/tests/fixtures/host_driver.c:826-844`):
//!
//! | opcode | flag | `*rbuf` | `cres` | the C's answer |
//! |---|---|---|---|---|
//! | `26` | clear | `NULL` | 0 | `[]` (`io.c:4134`) |
//! | `27` | clear | `NULL` | 5 | `[]` — the pointer test precedes any length use |
//! | `28` | **set** | `NULL` | 0 | `[]`, **not** `<<>>` (`io.c:4134` before `:4136`) |
//! | `29` | **set** | buffer kept | 0 | `<<>>` (`io.c:4136-4162`) |
//! | `30` | clear | buffer kept | 0 | `[]` (`io.c:4163-4166`, `buf_to_intlist` of nothing) |
//!
//! `set_port_control_flags` (`io.c:5318`) needs the `ErlDrvPort` `start` is handed and the control
//! callback never sees, so `start` stashes it in a file-static (`rt_port` in the fixture). The flag
//! is per-port **state**, which is why every shape verb states it explicitly rather than relying on
//! the row before it.//!
//! A second build of the same source (`RT_CTL_HAS_CONTROL=0`) leaves the entry's `control` slot
//! `NULL`, which is how the absent-slot `badarg` is discriminated.
//!
//! # Skip policy
//!
//! Needs the host C compiler and the host `erl_driver.h`; without either it prints a **loud**
//! `SKIP:` banner and returns — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_io::registry::PortRegistry;
use beam_io::Poller;
use beam_process::{PortId, ProcessId};
use beam_term::AtomTable;

use super::{DriverPort, DriverPortAdapter};

/// The fixture: a driver whose `control` callback answers one of ten verbs (see the module docs).
///
/// `start` only stashes the port handle and (when it is handed one) records the marker path, so
/// the port has no descriptor and no timer — this suite is about the control path alone.
const CONTROL_C: &str = r#"
#include <erl_driver.h>

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define RT_CTL_MARKER_MAX 512

/* The verbs this test drives. */
#define RT_CTL_REVERSE  1
#define RT_CTL_FIXED    2
#define RT_CTL_NEGATIVE 3
#define RT_CTL_NIL      4
#define RT_CTL_FOREIGN  5
/* The reply-shape verbs — `beam_io`'s own numbering for these exact shapes
 * (`crates/beam_io/tests/fixtures/host_driver.c:826-844`). */
#define RT_CTL_NULL_NO_FLAG        26
#define RT_CTL_NULL_NO_FLAG_LENGTH 27
#define RT_CTL_NULL_BINARY         28
#define RT_CTL_EMPTY_BINARY        29
#define RT_CTL_EMPTY_LIST          30

#define RT_FIXED_REPLY   "CTL!"
#define RT_FOREIGN_REPLY "FOREIGN"

/* A buffer **inside this object**: a reply pointing at it is addressable neither through the reply
 * buffer the runtime handed in nor through the request buffer. */
static char rt_foreign[sizeof(RT_FOREIGN_REPLY)] = RT_FOREIGN_REPLY;

/* The port handle `start` is handed. `set_port_control_flags` takes an `ErlDrvPort` and the
 * control callback only ever sees the opaque `ErlDrvData`, so the handle is kept here. */
static ErlDrvPort rt_port = NULL;

/* The `set_port_control_flags` entry point, resolved in `start`.
 *
 * A real driver just calls `set_port_control_flags` and lets the loader bind the import: an
 * emulator publishes its driver C ABI as **dynamic** symbols (`nm -D beam.smp` shows
 * `T set_port_control_flags`), which is how every shipped driver resolves it. The host `erl` probe
 * below therefore needs nothing special from this fixture — and this suite compiles **one** source
 * for both runtimes, so the import cannot simply be a direct call: this crate's `--lib` unit-test
 * binary publishes **no** global dynamic symbol at all (cargo's `rustc-link-arg-tests` reaches
 * `[[test]]` targets, not the lib's unit-test harness, so `readelf --dyn-syms` lists only
 * `UND` entries), and a `.so` that *imports* the symbol cannot be `dlopen`ed there
 * (`undefined symbol: set_port_control_flags`). The entry point is therefore looked up twice, and
 * both routes call the host's own implementation — never a stub:
 *
 *   * the address the host hands in the `start` command (this runtime's suites, see `rt_start`),
 *     which is the same convention `port_attributes_tests.rs` uses for `set_busy_port`;
 *   * otherwise the loader (`dlsym(RTLD_DEFAULT, …)`), which is the route a real emulator takes. */
typedef void (*rt_set_flags_fn)(ErlDrvPort, int);
static rt_set_flags_fn rt_set_flags = NULL;

/* The marker file, opened in `start` and appended to on every control call. */
static FILE *rt_marker = NULL;

/* `start`'s `command` is "<object path or driver name> [<entry-point address in hex> [<marker>]]".
 * Every argument after the first is optional: the end-to-end oracle probe opens the driver by bare
 * name (no address, no marker) and relies on the loader lookup above, while this crate's own
 * suites hand both. */
static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    char object[RT_CTL_MARKER_MAX];
    char marker[RT_CTL_MARKER_MAX];
    unsigned long address = 0;
    int fields;
    object[0] = '\0';
    marker[0] = '\0';
    rt_port = port;
    fields = command == NULL ? 0 : sscanf(command, "%511s %lx %511s", object, &address, marker);
    if (fields >= 2)
        rt_set_flags = (rt_set_flags_fn) (intptr_t) address;
    else
        rt_set_flags = (rt_set_flags_fn) dlsym(RTLD_DEFAULT, "set_port_control_flags");
    if (fields < 3)
        marker[0] = '\0';
    /* A marker failure no longer fails `start`: the suites that care about the marker assert the
     * lines it wrote (an unopened marker shows up there), and the end-to-end suite opens the same
     * driver from an Erlang probe that passes no marker at all. */
    if (marker[0] != '\0')
        rt_marker = fopen(marker, "w");
    return (ErlDrvData) 1;  /* any non-negative private state */
}
static void rt_stop(ErlDrvData drv_data) {
    (void) drv_data;
}

/* State the port's control flags through the resolved entry point — the C's
 * `set_port_control_flags(prt, flags)` (`io.c:5318`). A host that supplied neither an address nor
 * an exported symbol answers `0`, and the caller returns the C's rejected `cres` (`-1`,
 * `io.c:4074`) rather than a row whose flag is silently not what the row claims. */
static int rt_state_flags(int flags) {
    if (rt_set_flags == NULL)
        return 0;
    rt_set_flags(rt_port, flags);
    return 1;
}
static ErlDrvSSizeT rt_control(ErlDrvData drv_data, unsigned int cmd, char *buf,
                               ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    ErlDrvSizeT i;
    ErlDrvSizeT n;
    (void) drv_data;
    if (rt_marker != NULL) {
        /* What the driver was *asked*: the verb as the runtime parsed it, and the request length. */
        fprintf(rt_marker, "C:%u:%lu\n", cmd, (unsigned long) len);
        fflush(rt_marker);
    }
    switch (cmd) {
    case RT_CTL_REVERSE:
        /* A reply that is not a prefix of the request, so an implementation that echoed the
         * caller's buffer would fail the assertion. */
        n = len < rlen ? len : rlen;
        for (i = 0; i < n; i++)
            (*rbuf)[i] = buf[len - 1 - i];
        return (ErlDrvSSizeT) n;
    case RT_CTL_FIXED:
        memcpy(*rbuf, RT_FIXED_REPLY, sizeof(RT_FIXED_REPLY) - 1);
        return (ErlDrvSSizeT) (sizeof(RT_FIXED_REPLY) - 1);
    case RT_CTL_NEGATIVE:
        return (ErlDrvSSizeT) -1;
    case RT_CTL_NIL:
        /* The C's `NIL`: no reply buffer at all. */
        *rbuf = NULL;
        return (ErlDrvSSizeT) 0;
    case RT_CTL_FOREIGN:
        /* A reply from this object's own memory — the runtime cannot read it without raw access. */
        *rbuf = rt_foreign;
        return (ErlDrvSSizeT) (sizeof(RT_FOREIGN_REPLY) - 1);
    case RT_CTL_NULL_NO_FLAG:
        /* `NULL` reply pointer, flag clear, `cres == 0`: the C's `NIL` (`io.c:4134`). */
        if (!rt_state_flags(0))
            return (ErlDrvSSizeT) -1;
        if (rbuf != NULL)
            *rbuf = NULL;
        return (ErlDrvSSizeT) 0;
    case RT_CTL_NULL_NO_FLAG_LENGTH:
        /* The same `NULL` pointer with a **non-zero** `cres`: still `NIL`, because the pointer
         * test precedes any use of the length (`io.c:4134`). */
        if (!rt_state_flags(0))
            return (ErlDrvSSizeT) -1;
        if (rbuf != NULL)
            *rbuf = NULL;
        return (ErlDrvSSizeT) 5;
    case RT_CTL_NULL_BINARY:
        /* `PORT_CONTROL_FLAG_BINARY` set **and** `*rbuf = NULL`: the empty list `[]`, never `<<>>`
         * — the row a runtime that reads the flag alone gets wrong. */
        if (!rt_state_flags(PORT_CONTROL_FLAG_BINARY))
            return (ErlDrvSSizeT) -1;
        if (rbuf != NULL)
            *rbuf = NULL;
        return (ErlDrvSSizeT) 0;
    case RT_CTL_EMPTY_BINARY:
        /* `PORT_CONTROL_FLAG_BINARY` set, zero bytes, the caller's buffer kept: `<<>>`. */
        if (!rt_state_flags(PORT_CONTROL_FLAG_BINARY))
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) 0;
    case RT_CTL_EMPTY_LIST:
        /* Flag cleared, zero bytes, the caller's buffer kept: `[]` again. */
        if (!rt_state_flags(0))
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) 0;
    default:
        return (ErlDrvSSizeT) -1;
    }
}
static ErlDrvEntry rt_control_entry = {
    .init = NULL,
    .start = rt_start,
    .stop = rt_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = RT_CONTROL_NAME,
    .finish = NULL,
    .handle = NULL,
#if RT_CONTROL_MODE
    .control = rt_control,
#else
    .control = NULL,
#endif
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

DRIVER_INIT(rt_control_init) {
    return &rt_control_entry;
}
"#;

/// Print a loud skip banner (never a silent pass).
pub(super) fn skip(reason: &str) {
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
/// A compact duplicate of `stop_select_tests.rs::erl_include_dir` (a unit test cannot import the
/// `tests/` crate target's support). Resolution order: ask the host `erl`; then the `include`
/// sibling of the `erts-*` directory the `erl` on `$PATH` lives in; then the standard roots.
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

/// A compiled driver `.so` plus the marker file its `control` callback appends to.
pub(super) struct Fixture {
    /// The scratch directory holding the source, the `.so` and the marker.
    pub(super) dir: PathBuf,
    /// The `.so` **without** its suffix (the `DriverPortAdapter::load` candidate).
    pub(super) stem: PathBuf,
    /// The path the fixture records each `control` call in.
    pub(super) marker: PathBuf,
    /// The `driver_name` the library is compiled under (the loader rejects a mismatch).
    pub(super) name: String,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output and the marker are not source).
    pub(super) fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The `C:<cmd>:<len>` lines the fixture has written so far, in order.
    ///
    /// Read after the callback has returned — the fixture `fflush`es each line as it writes it.
    pub(super) fn marker_lines(&self) -> Vec<String> {
        match std::fs::read_to_string(&self.marker) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            // A missing marker means the fixture never got as far as `start`: report it as no
            // lines so the caller's assertion, not a panic, is what fails.
            Err(_) => Vec::new(),
        }
    }
}

/// Compiles [`CONTROL_C`] as `<scratch>/<stem>.so`; `None` when a host facility is missing (the
/// caller returns early — **loudly**). `with_control` selects the entry's `control` slot: `false`
/// compiles the absent-slot build.
pub(super) fn build_fixture(stem: &str, with_control: bool) -> Option<Fixture> {
    if !cc_available() {
        skip(&format!("control: no C compiler ({} / $CC) available", cc()));
        return None;
    }
    let Some(include) = erl_include_dir() else {
        skip("control: no erl_driver.h include directory found");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_control_{stem}_{}_{n}", std::process::id()));
    let source = dir.join(format!("{stem}.c"));
    let write =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, CONTROL_C));
    if let Err(error) = write {
        skip(&format!("control: cannot prepare {}: {error}", dir.display()));
        return None;
    }
    // `cc -shared -fPIC -DRT_CONTROL_NAME="<stem>" -DRT_CONTROL_MODE=<0|1> -I<include> -o <so> <c>`
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-DRT_CONTROL_NAME=\"{stem}\""))
        .arg(format!("-DRT_CONTROL_MODE={}", u8::from(with_control)))
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join(format!("{stem}.so")))
        .arg(&source)
        .output();
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            skip(&format!(
                "control: cc failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("control: could not run the C compiler: {error}"));
            return None;
        }
    }
    if !dir.join(format!("{stem}.so")).is_file() {
        skip(&format!("control: the C compiler produced no {stem}.so"));
        return None;
    }
    Some(Fixture {
        stem: dir.join(stem),
        marker: dir.join("marker.txt"),
        dir,
        name: stem.to_string(),
    })
}

// ---------------------------------------------------------------------------------------------
// Driving the port
// ---------------------------------------------------------------------------------------------

/// Loads `fixture`'s driver into an adapter — the port is **not** opened yet.
///
/// `start` is handed `"<object> <entry-point address> <marker>"` (the fixture's `rt_start`): the
/// marker path it records every control call in (see the module docs) and the address of the real
/// [`set_port_control_flags`](beam_abi::driver::drv::lifecycle::set_port_control_flags) entry
/// point, because this unit-test binary exports **no** dynamic symbol for the `.so` to import
/// (`readelf --dyn-syms` lists only `UND` entries: cargo's `rustc-link-arg-tests` reaches
/// `[[test]]` targets, not the lib's unit-test harness). The same source is compiled for the
/// oracle probe in [`control_reply_oracle_tests`](super::control_reply_oracle_tests), which opens
/// it by bare name and lets the loader find the emulator's exported symbol instead.
fn load_adapter(fixture: &Fixture) -> DriverPortAdapter {
    // The same anchor `crate::port::PortRegistry::new` uses.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let atoms = Rc::new(AtomTable::new());
    let host = DriverPort::new(
        Heap::new(256).expect("a test heap"),
        &atoms,
        ProcessId::new(1, 0),
    );
    let command = format!(
        "{} {:x} {}",
        fixture.stem.with_extension("so").display(),
        set_port_control_flags_address(),
        fixture.marker.display()
    );
    DriverPortAdapter::load(
        &[fixture.stem.display().to_string()],
        &fixture.name,
        host,
        command,
    )
    .expect("the fixture driver loads")
}

/// The address of the real `set_port_control_flags` entry point, handed to the fixture's `start`
/// — the same convention `port_attributes_tests.rs` uses for `set_busy_port`/`erl_drv_set_os_pid`.
pub(super) fn set_port_control_flags_address() -> usize {
    beam_abi::driver::drv::driver_api_table()
        .set_port_control_flags
        .expect("the `set_port_control_flags` entry point is registered in the table")
        as usize
}

/// Loads `fixture`'s driver and opens one port on it.
fn open_port(fixture: &Fixture) -> (Poller, PortRegistry, PortId) {
    let adapter = load_adapter(fixture);
    let mut poller = Poller::new().expect("a poller");
    let mut ports = PortRegistry::new();
    let id = ports
        .open(&mut poller, Box::new(adapter))
        .expect("the port opens (the fixture's `start` opened the marker)");
    (poller, ports, id)
}
// ---------------------------------------------------------------------------------------------
// The end-to-end cases (a real C driver, driven through `PortRegistry::port_control`)
// ---------------------------------------------------------------------------------------------

/// A control command reaches the driver's `control` callback and **its** reply bytes come back.
///
/// Discriminates: the fixture reverses the request, so a reply that merely echoed the caller's own
/// buffer (or a default no-reply `Ok(vec![])`) fails; the marker proves the verb arrived as the
/// parsed integer `1` and the request as its real 5 bytes.
#[test]
fn a_control_command_reaches_the_driver_and_its_reply_comes_back() {
    let Some(fixture) = build_fixture("rt_control_reverse", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    let reply = ports
        .port_control(&mut poller, id, "1", b"hello")
        .expect("the driver's `control` answers");
    assert_eq!(reply, b"olleh", "the callback's own reply bytes");

    assert_eq!(
        fixture.marker_lines(),
        vec!["C:1:5".to_string()],
        "the verb reached the driver as the parsed integer 1, with the real request length"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// A reply independent of the request is returned **verbatim** — the driver's buffer, not a slice
/// of the request.
#[test]
fn a_reply_the_driver_writes_into_the_handed_buffer_comes_back() {
    let Some(fixture) = build_fixture("rt_control_fixed", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    let reply = ports
        .port_control(&mut poller, id, "2", b"ignored")
        .expect("the driver's `control` answers");
    assert_eq!(reply, b"CTL!");

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// A negative `cres` is the C's `ERTS_PORT_OP_BADARG` (`io.c:4074`).
#[test]
fn a_negative_control_result_is_badarg() {
    let Some(fixture) = build_fixture("rt_control_negative", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    let error = ports
        .port_control(&mut poller, id, "3", b"")
        .expect_err("a negative result is badarg");
    assert!(matches!(error, BeamError::BadArg), "got {error:?}");
    assert_eq!(fixture.marker_lines(), vec!["C:3:0".to_string()]);

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// A `NULL` reply pointer is the C's `NIL` (`io.c:4126`): an **empty reply**, not an error —
/// which is what distinguishes it from the absent-slot and negative-result failures.
///
/// Re-derived: the bytes alone do not say *why* they are empty, so the test also pins the fact the
/// renderer needs beside them — [`PortRegistry::control_reply_shape`] is
/// [`ControlReplyShape::Null`](beam_io::ControlReplyShape::Null) for this reply, and `Null` is what
/// makes `erlang:port_control/3` answer `[]` for **every** `PORT_CONTROL_FLAG_BINARY` setting
/// (`io.c:4134`) instead of the `<<>>` a zero-byte *buffer* would become under that flag.
#[test]
fn a_null_reply_pointer_is_an_empty_reply_not_an_error() {
    let Some(fixture) = build_fixture("rt_control_nil", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    let reply = ports
        .port_control(&mut poller, id, "4", b"")
        .expect("the C's NIL is an empty reply");
    assert!(reply.is_empty(), "no fabricated bytes: {reply:?}");
    assert_eq!(
        ports
            .control_reply_shape(id)
            .expect("the port is live, so its last reply is describable"),
        beam_io::ControlReplyShape::Null,
        "the driver left *rbuf NULL: the C tests that pointer first (`io.c:4134`)"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// The **two zero-byte reply shapes** `write_port_control_result` keeps apart
/// (`io.c:4126-4175`), driven through a real C driver: a `NULL` reply pointer and a zero-byte
/// **buffer** both carry no bytes, so only the shape tells them apart — and the C renders the first
/// as `[]` for **every** `PORT_CONTROL_FLAG_BINARY` setting (`io.c:4134`, the pointer test comes
/// *before* the flag test at `io.c:4136`) while the second is `<<>>` under that flag
/// (`io.c:4136-4162`) and `[]` without it (`io.c:4163-4166`, `buf_to_intlist` of nothing).
///
/// Every row is a distinct verb of the fixture, and every verb states the port's flag word
/// explicitly (it is per-port state, `io.c:5318`). The rows, in drive order:
///
/// | verb | flag | `*rbuf` | `cres` | shape | the C's answer |
/// |---|---|---|---|---|---|
/// | `28` | set | `NULL` | 0 | `Null` | `[]` — the row a flag-only reading renders `<<>>` |
/// | `29` | set | buffer | 0 | `Buffer` | `<<>>` |
/// | `30` | clear | buffer | 0 | `Buffer` | `[]` |
/// | `26` | clear | `NULL` | 0 | `Null` | `[]` |
/// | `27` | clear | `NULL` | 5 | `Null` | `[]` — `cres` is irrelevant once the pointer is `NULL` |
///
/// Discriminates: an implementation that derived the shape from `cres == 0` (or from the bytes)
/// would answer `Buffer` for verbs `28`, `26` and `27`, and one that never latched the pointer at
/// all would answer `Buffer` for every row.
#[test]
fn the_reply_shape_separates_a_null_pointer_from_a_zero_byte_buffer() {
    use beam_io::ControlReplyShape;

    let Some(fixture) = build_fixture("rt_control_shape", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    // Every row reports the shape (`PortRegistry::control_reply_shape`) beside its bytes; `flag`
    // says whether the verb set `PORT_CONTROL_FLAG_BINARY` on the port (`io.c:5318`).
    let row = |ports: &mut PortRegistry, poller: &mut Poller, verb: &str, flag: bool| {
        let bytes = ports
            .port_control(poller, id, verb, b"")
            .unwrap_or_else(|error| panic!("verb {verb} must reach the driver: {error}"));
        assert!(
            bytes.is_empty(),
            "verb {verb} answers a zero-length reply: {bytes:?}"
        );
        assert_eq!(
            ports
                .control_reply_shape(id)
                .expect("the port is live, so its last reply is describable"),
            if flag {
                ControlReplyShape::Buffer
            } else {
                ControlReplyShape::Null
            },
            "verb {verb}: the reply pointer decides the shape (`io.c:4134`), never `cres` \
             (both rows are zero bytes)"
        );
    };

    // Row: `PORT_CONTROL_FLAG_BINARY` set + `*rbuf = NULL` ⇒ `[]`, never `<<>>` (`io.c:4134`
    // precedes the flag test at `io.c:4136`).
    row(&mut ports, &mut poller, "28", false);
    // Row: the flag set + a zero-byte **buffer** ⇒ `<<>>` (`io.c:4136-4162`). The same zero bytes
    // as verb 28 and the opposite shape.
    row(&mut ports, &mut poller, "29", true);
    // Row: the flag cleared + a zero-byte buffer ⇒ `[]` (`io.c:4163-4166`).
    row(&mut ports, &mut poller, "30", true);
    // Row: the flag clear + `NULL` ⇒ `[]` (`io.c:4134`), shape `Null` though the flag is off.
    row(&mut ports, &mut poller, "26", false);
    // Row: `NULL` with a non-zero `cres` ⇒ still `[]`: the pointer is tested before any length use.
    row(&mut ports, &mut poller, "27", false);

    assert_eq!(
        fixture.marker_lines(),
        vec![
            "C:28:0".to_string(),
            "C:29:0".to_string(),
            "C:30:0".to_string(),
            "C:26:0".to_string(),
            "C:27:0".to_string(),
        ],
        "every verb reached the driver as the parsed integer, with an empty request"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// The latched shape describes the **last successful** `control` call
/// ([`DriverPortAdapter::control`] sets it only once `cres` was accepted, `io.c:4074`), so it
/// follows the driver from row to row instead of latching on the first `NULL`.
///
/// Discriminates: a latch written only when the pointer is `NULL` (or only once) would answer
/// `Null` for the second call below.
#[test]
fn the_reply_shape_follows_the_last_successful_control_call() {
    use beam_io::ControlReplyShape;

    let Some(fixture) = build_fixture("rt_control_shape_follow", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);
    let shape = |ports: &PortRegistry| {
        ports
            .control_reply_shape(id)
            .expect("the port is live, so its last reply is describable")
    };

    // A `NULL` reply (verb 28) latches `Null`...
    assert!(ports
        .port_control(&mut poller, id, "28", b"")
        .expect("the driver answers")
        .is_empty());
    assert_eq!(shape(&ports), ControlReplyShape::Null);
    // ...and the next call, which answers from a real buffer (verb 2), latches `Buffer` again.
    assert_eq!(
        ports
            .port_control(&mut poller, id, "2", b"ignored")
            .expect("the driver answers"),
        b"CTL!"
    );
    assert_eq!(
        shape(&ports),
        ControlReplyShape::Buffer,
        "the shape describes the last call, not the first NULL one"
    );
    // A call whose `cres` is **rejected** (`io.c:4074`, verb 3) does not re-latch: the shape still
    // describes the last *successful* call (`io.c:4134` is only reached for an accepted reply).
    assert!(matches!(
        ports
            .port_control(&mut poller, id, "3", b"")
            .expect_err("a negative result is badarg"),
        BeamError::BadArg
    ));
    assert_eq!(
        shape(&ports),
        ControlReplyShape::Buffer,
        "a rejected call leaves the last successful call's shape in place"
    );
    // A buffer reply (verb 30) followed by a `NULL` one (verb 4) flips the shape back to `Null`.
    assert!(ports
        .port_control(&mut poller, id, "30", b"")
        .expect("the driver answers")
        .is_empty());
    assert_eq!(shape(&ports), ControlReplyShape::Buffer);
    assert!(ports
        .port_control(&mut poller, id, "4", b"")
        .expect("the driver answers")
        .is_empty());
    assert_eq!(
        shape(&ports),
        ControlReplyShape::Null,
        "a NULL reply latches Null whatever the call before it answered"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}
/// A reply the driver answers from **its own** memory is the typed capability gap — never a
/// fabricated or truncated reply.
///
/// Discriminates: the pointer addresses neither the reply buffer the runtime handed in nor the
/// request buffer, so no safe read can reach the bytes; the honest answer is `unsupported`, and an
/// implementation that dropped them would fail here by returning `Ok`.
#[test]
fn a_reply_from_the_drivers_own_memory_is_a_typed_gap() {
    let Some(fixture) = build_fixture("rt_control_foreign", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    let error = ports
        .port_control(&mut poller, id, "5", b"")
        .expect_err("a reply from the driver's own memory cannot be read");
    let reason = crate::error::unsupported_reason(&error).expect("a capability gap");
    assert!(
        reason.contains("driver's own memory"),
        "the gap names the reason: {reason}"
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// An entry with no `control` slot is the C's `ERTS_PORT_OP_BADARG` (`io.c:4035`) — not an empty
/// reply.
#[test]
fn an_entry_without_a_control_slot_is_badarg() {
    let Some(fixture) = build_fixture("rt_control_absent", false) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    let error = ports
        .port_control(&mut poller, id, "1", b"hello")
        .expect_err("an absent `control` slot is badarg");
    assert!(matches!(error, BeamError::BadArg), "got {error:?}");

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// The **capability** the runtime captures at `open_port/2` for the `port_control/3` gate is the
/// loaded entry's own `control` slot: `true` when the slot is declared, `false` for the `NULL`
/// build — the C's `!prt->drv_ptr->control` (`io.c:4035`).
///
/// That captured bit (not a driver **name**) is what
/// [`PortMeta::has_driver_control`](crate::port::PortMeta::has_driver_control) answers from, so a
/// `.so` driver with no `control` callback is refused for every verb without the runtime knowing
/// anything about the driver's name.
#[test]
fn the_captured_control_capability_is_the_loaded_entrys_slot() {
    let Some(present) = build_fixture("rt_control_present", true) else {
        return;
    };
    {
        let adapter = load_adapter(&present);
        assert!(
            beam_io::Driver::has_control(&adapter),
            "an entry that declares `control` declares the capability"
        );
    }
    present.cleanup();

    let Some(absent) = build_fixture("rt_control_slotless", false) else {
        return;
    };
    {
        let adapter = load_adapter(&absent);
        assert!(
            !beam_io::Driver::has_control(&adapter),
            "an entry whose `control` slot is NULL declares none"
        );
    }
    absent.cleanup();
}
/// A verb that is not a decimal `c_uint` is the caller's `badarg` — the driver is **never**
/// entered, so no command is substituted for it.
#[test]
fn a_verb_that_is_not_a_decimal_uint_is_badarg_before_the_driver_runs() {
    let Some(fixture) = build_fixture("rt_control_verb", true) else {
        return;
    };
    let (mut poller, mut ports, id) = open_port(&fixture);

    let error = ports
        .port_control(&mut poller, id, "echo", b"hello")
        .expect_err("a non-numeric verb is badarg");
    assert!(matches!(error, BeamError::BadArg), "got {error:?}");
    assert!(
        fixture.marker_lines().is_empty(),
        "the driver was never entered: {:?}",
        fixture.marker_lines()
    );

    ports.close(&mut poller, id).expect("the port closes");
    fixture.cleanup();
}

/// A port this registry does not hold answers `badarg` — `beam_io`'s own answer, and the oracle's
/// for `port_control` on a closed port. No bytes are ever invented for it.
#[test]
fn an_unknown_port_is_badarg() {
    let mut poller = Poller::new().expect("a poller");
    let mut ports = PortRegistry::new();
    let error = ports
        .port_control(&mut poller, PortId::new(9999), "1", b"")
        .expect_err("an unknown port is badarg");
    assert!(matches!(error, BeamError::BadArg), "got {error:?}");
}

// ---------------------------------------------------------------------------------------------
// The reply-source rule (no C fixture needed)
// ---------------------------------------------------------------------------------------------

/// A `NULL` reply pointer is the C's `NIL` — an empty reply (`io.c:4126`).
#[test]
fn a_null_reply_pointer_is_empty() {
    let reply = [0u8; 8];
    let request = [9u8; 4];
    let bytes = DriverPortAdapter::control_reply(
        &reply,
        &request,
        std::ptr::null::<std::os::raw::c_char>(),
        0,
    )
    .expect("a NULL reply pointer is the C's NIL, not a failure");
    assert!(bytes.is_empty());
}

/// A reply pointer inside the **reply buffer the runtime handed in** is read from that buffer.
#[test]
fn a_reply_inside_the_handed_buffer_is_read_from_it() {
    let reply: Vec<u8> = (0u8..16).collect();
    let request = vec![b'x'; 4];
    // The driver wrote at offset 2 and reported 3 bytes (a safe way to address `&reply[2]`).
    let pointer = reply[2..].as_ptr() as *const std::os::raw::c_char;
    let bytes = DriverPortAdapter::control_reply(&reply, &request, pointer, 3)
        .expect("the handed buffer is addressable");
    assert_eq!(bytes, reply[2..5].to_vec());
}

/// A reply pointer inside the **request buffer** is read from it too (the C reads `*rbuf`
/// whatever it points at, and that buffer is the caller's for the duration of the call).
#[test]
fn a_reply_inside_the_request_buffer_is_read_from_it() {
    let reply = vec![0u8; 4];
    let request = b"abcdefgh".to_vec();
    let pointer = request.as_ptr() as *const std::os::raw::c_char;
    let bytes = DriverPortAdapter::control_reply(&reply, &request, pointer, 8)
        .expect("the request buffer is addressable");
    assert_eq!(bytes, b"abcdefgh".to_vec());
}

/// A reply that runs **past the end** of the buffer it points into is the typed gap, not a
/// truncated read: the runtime may not read beyond a buffer it owns.
#[test]
fn a_reply_that_runs_past_the_buffer_is_a_typed_gap() {
    let reply = vec![0u8; 4];
    let request = vec![0u8; 4];
    let pointer = reply.as_ptr() as *const std::os::raw::c_char;
    let error = DriverPortAdapter::control_reply(&reply, &request, pointer, 5)
        .expect_err("5 bytes do not fit a 4-byte buffer");
    assert!(crate::error::unsupported_reason(&error).is_some(), "got {error:?}");
}

/// A pointer that addresses neither buffer is the typed gap — the driver's own memory.
#[test]
fn a_reply_from_a_foreign_pointer_is_a_typed_gap() {
    let reply = vec![0u8; 4];
    let request = vec![0u8; 4];
    // A pointer that cannot be inside either local buffer.
    let pointer = 0x1000 as *const std::os::raw::c_char;
    let error = DriverPortAdapter::control_reply(&reply, &request, pointer, 1)
        .expect_err("a foreign pointer cannot be read");
    let reason = crate::error::unsupported_reason(&error).expect("a capability gap");
    assert!(reason.contains("driver's own memory"), "{reason}");
}
