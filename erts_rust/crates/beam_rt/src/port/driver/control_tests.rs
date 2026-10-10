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
//! The reply verbs are chosen so the reply cannot be confused with the request: `RT_CTL_REVERSE`
//! **reverses** the request (so a reply that happened to echo the caller's own buffer would fail),
//! `RT_CTL_FIXED` answers a constant, `RT_CTL_NIL` nulls the reply pointer (the C's `NIL`),
//! `RT_CTL_FOREIGN` points it at a buffer **inside the loaded object**, and `RT_CTL_NEGATIVE`
//! returns `-1`.
//!
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

/// The fixture: a driver whose `control` callback answers one of five verbs (see the module docs).
///
/// `start` only records the marker path, so the port has no descriptor and no timer — this suite
/// is about the control path alone.
const CONTROL_C: &str = r#"
#include <erl_driver.h>

#include <stdio.h>
#include <string.h>

#define RT_CTL_MARKER_MAX 512

/* The verbs this test drives. */
#define RT_CTL_REVERSE  1
#define RT_CTL_FIXED    2
#define RT_CTL_NEGATIVE 3
#define RT_CTL_NIL      4
#define RT_CTL_FOREIGN  5

#define RT_FIXED_REPLY   "CTL!"
#define RT_FOREIGN_REPLY "FOREIGN"

/* A buffer **inside this object**: a reply pointing at it is addressable neither through the reply
 * buffer the runtime handed in nor through the request buffer. */
static char rt_foreign[sizeof(RT_FOREIGN_REPLY)] = RT_FOREIGN_REPLY;

/* The marker file, opened in `start` and appended to on every control call. */
static FILE *rt_marker = NULL;

static ErlDrvData rt_start(ErlDrvPort port, char *command) {
    char marker[RT_CTL_MARKER_MAX];
    (void) port;
    if (command == NULL)
        return (ErlDrvData) -1;
    if (sscanf(command, "%511s", marker) != 1)
        return (ErlDrvData) -1;
    rt_marker = fopen(marker, "w");
    if (rt_marker == NULL)
        return (ErlDrvData) -1;
    return (ErlDrvData) 1;  /* any non-negative private state */
}

static void rt_stop(ErlDrvData drv_data) {
    (void) drv_data;
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
struct Fixture {
    /// The scratch directory holding the source, the `.so` and the marker.
    dir: PathBuf,
    /// The `.so` **without** its suffix (the `DriverPortAdapter::load` candidate).
    stem: PathBuf,
    /// The path the fixture records each `control` call in.
    marker: PathBuf,
    /// The `driver_name` the library is compiled under (the loader rejects a mismatch).
    name: String,
}

impl Fixture {
    /// Removes the scratch directory (`cc` output and the marker are not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }

    /// The `C:<cmd>:<len>` lines the fixture has written so far, in order.
    ///
    /// Read after the callback has returned — the fixture `fflush`es each line as it writes it.
    fn marker_lines(&self) -> Vec<String> {
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
fn build_fixture(stem: &str, with_control: bool) -> Option<Fixture> {
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

/// Loads `fixture`'s driver and opens one port on it.
///
/// The fixture is handed the marker path in the `command` string (see the module docs), which its
/// `start` reads.
fn open_port(fixture: &Fixture) -> (Poller, PortRegistry, PortId) {
    // The same anchor `crate::port::PortRegistry::new` uses.
    let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
    let atoms = Rc::new(AtomTable::new());
    let host = DriverPort::new(Heap::new(256).expect("a test heap"), atoms, ProcessId::new(1, 0));
    let adapter = DriverPortAdapter::load(
        &[fixture.stem.display().to_string()],
        &fixture.name,
        host,
        fixture.marker.display().to_string(),
    )
    .expect("the fixture driver loads");
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
