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

//! End-to-end proof of the **port-closing family** of the driver C ABI (workspace contract
//! **A.4**) — `driver_failure_eof`, `driver_failure_atom`, `driver_failure_posix`,
//! `driver_failure` and `driver_exit`: the real C driver `tests/fixtures/driver_failure.c`,
//! compiled with the system `cc` against the system `erl_driver.h`, is `dlopen`ed, validated and
//! driven through this crate's loader, and the **very same `.so`** is then loaded and driven by the
//! **real system ERTS**.
//!
//! # The two obligations the C imposes, and where each is proven
//!
//! Every spelling funnels through `driver_failure_term(ix, term, eof)`
//! (`erts/emulator/beam/io.c:7196-7229`, entries at `io.c:7234`/`7258`/`7263`/`7273`/`7278`), which
//! does two things the crate cannot do itself:
//!
//! 1. **It hands the runtime one of four reason shapes** — `Eof` (`normal`), `Atom(name)`,
//!    `Posix(errno)` (the lowercase POSIX-name atom) or `Error(code)` (a bare integer), plus
//!    `driver_exit`'s unlink-then-close. This crate's half of the contract is that each C entry
//!    point lands on exactly one [`DriverHost`] hook with exactly that reason, and that an invalid
//!    port operand reaches no hook at all: [`a_real_c_driver_hands_every_spelling_to_one_host_hook`].
//! 2. **It unlinks the connected process, delivers `{'EXIT',Port,Reason}` (or, to a monitor,
//!    `{'DOWN',…}`) and leaves the port closed.** That is the *host's* job (no production host
//!    implements it yet — see the crate `CONTEXT.md`'s handoffs), so here it is pinned as the
//!    measured behaviour of the real ERTS for the same `.so`:
//!    [`the_oracle_observes_every_driver_failure_reason_and_the_closed_port`].
//!
//! The two halves are tied together: the oracle assertions derive the expected message from the
//! reason shape **this crate** hands the host (including `erl_errno_id`'s answer for
//! `PortFailure::Posix`), so the reason terms are proven identical across the runtimes rather than
//! written down twice.
//!
//! # Measured truth (OTP 29.1.1 / erts-17.1)
//!
//! With a `trap_exit` owner (or, for the `monitor` column, an `unlink/1` + `monitor(port, …)`):
//!
//! | command | C entry point | control return | owner sees | monitor sees | port after |
//! |---|---|---|---|---|---|
//! | `1` | `driver_failure_eof(P)` | `0` | `{'EXIT',P,normal}` | `{'DOWN',…,normal}` | closed |
//! | `2` | `driver_failure_atom(P,"boom")` | `0` | `{'EXIT',P,boom}` | `{'DOWN',…,boom}` | closed |
//! | `3` | `driver_failure_posix(P,EACCES)` | `0` | `{'EXIT',P,eacces}` | `{'DOWN',…,eacces}` | closed |
//! | `4` | `driver_failure(P,7)` | `0` | `{'EXIT',P,7}` | `{'DOWN',…,7}` | closed |
//! | `5` | `driver_failure(P,0)` | `0` | `{'EXIT',P,normal}` | `{'DOWN',…,normal}` | closed |
//! | `6` | `driver_exit(P,0)` | `0` | **none** (unlinked first) | `{'DOWN',…,normal}` | closed |
//! | `7` | `driver_exit(P,EACCES)` | `0` | **none** | `{'DOWN',…,eacces}` | closed |
//! | `8` | `driver_failure_eof((ErlDrvPort)-1)` | `-1` | none | none | **alive** |
//!
//! `port_control/3` returns `[]` for a non-negative `control` return and raises `error:badarg` for
//! the negative one, so on command 8 the assertion is on the *port's* fate, never the return.
//! After a real close `port_info(P, name)` answers `undefined` and `port_close(P)` raises
//! `error:badarg`; the sentinel leaves both untouched (`{name,"drvfail"}` and `true`).
//!
//! # One fresh VM and one fresh port per spelling
//!
//! A second `driver_failure_*` from a callback on the port it just terminated is a measured
//! use-after-free inside the C (SIGSEGV on ~3 of 4 solo runs) — unpinnable, so the probe runs one
//! `erl` per command with a single port and never exercises that shape.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`); inspecting its symbol table needs `nm`/`readelf`; the oracle half additionally
//! needs `erlc`/`erl`. When a facility is missing the affected test prints a loud `SKIP:` banner and
//! returns — never a silent pass. When the toolchain **is** present every assertion is hard.

mod common;

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::host::PortFailure;
use beam_abi::driver::loader::{load, DriverLibrary};
use beam_abi::driver::types::ErlDrvPort;
use beam_abi::{DriverEnv, DriverHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{make_atom, Eterm};
use common::{compile_so, skip};
use libc::{c_char, c_int, c_uint};

/// The committed C fixture — a real driver (see the file header for the A.4 rationale).
const FIXTURE_C: &str = include_str!("fixtures/driver_failure.c");

/// The entry's `driver_name`, which is also the `.so` name: `erl_ddll:load_driver/2` and [`load`]
/// both reject a library whose `driver_name` differs from the requested name (`bad_driver_name`),
/// so the scratch `.so` is compiled with this stem.
const FIXTURE_NAME: &str = "drvfail";

/// The `driver_*` symbols the fixture imports. They must be *undefined* in the `.so` and *defined*
/// in the loading process (`build.rs`'s `-Wl,--export-dynamic` + the `#[used]` api table), so a
/// successful `dlopen(RTLD_NOW)` is itself evidence the family is exported — and the values the
/// fixture returns were produced by *this crate's* implementation.
const FIXTURE_IMPORTS: [&str; 7] = [
    "driver_alloc",
    "driver_free",
    "driver_failure_eof",
    "driver_failure_atom",
    "driver_failure_posix",
    "driver_failure",
    "driver_exit",
];

/// The `control` commands, in the order both halves drive them (the fixture's `DF_CMD_*`).
const COMMANDS: [c_uint; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

// ---------------------------------------------------------------------------------------------
// What each command must hand the host (and what it must not)
// ---------------------------------------------------------------------------------------------

/// One [`PortFailure`] reason, owned (the seam's `Atom` borrows the C string, which is gone once
/// the callback returns, so the recording host keeps a mirror).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recorded {
    /// The eof route (`driver_failure_eof`, and `driver_failure(P, 0)`).
    Eof,
    /// `driver_failure_atom`'s name.
    Atom(String),
    /// `driver_failure_posix`'s raw errno.
    Posix(c_int),
    /// `driver_failure(P, code)` with `code != 0`.
    Error(c_int),
}

/// The hook one command must reach, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reached {
    /// [`DriverHost::failure`] with this reason — and **not** `port_exit`.
    Failure(Recorded),
    /// [`DriverHost::port_exit`] with this raw code — and **not** `failure`.
    Exit(c_int),
    /// No hook at all (the invalid-port operand).
    Nothing,
}

/// The expected per-command outcome, in [`COMMANDS`] order.
///
/// Command 5 — `driver_failure(port, 0)` — is the **same request** as command 1: the C's third
/// argument to `driver_failure_term` is the `eof` flag, and it is `code == 0`, so the reason term
/// is discarded and the owner observes `normal` (`io.c:7258`). Command 8 is the invalid-port
/// sentinel, which the C answers `-1` for without touching the port (`io.c:7204`).
fn expected() -> Vec<Reached> {
    vec![
        Reached::Failure(Recorded::Eof),
        Reached::Failure(Recorded::Atom("boom".to_string())),
        Reached::Failure(Recorded::Posix(libc::EACCES)),
        Reached::Failure(Recorded::Error(7)),
        Reached::Failure(Recorded::Eof),
        Reached::Exit(0),
        Reached::Exit(libc::EACCES),
        Reached::Nothing,
    ]
}

/// The atom `driver_failure_atom(port, "boom")` must hand the host (the fixture's literal).
const EXPECTED_ATOM: &str = "boom";

// ---------------------------------------------------------------------------------------------
// The host: records the one hook each spelling reaches
// ---------------------------------------------------------------------------------------------

/// A [`DriverHost`] that records [`DriverHost::failure`] / [`DriverHost::port_exit`] and answers
/// the C's success code (`0`).
struct CloseHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
    /// Every `failure` reason, in call order.
    failures: Vec<Recorded>,
    /// Every `port_exit` code, in call order.
    exits: Vec<c_int>,
    /// The code both hooks answer (the C's `0` = ok; `-1` = failed).
    answer: c_int,
    /// Every byte string `driver_output` delivered (the family under test delivers none — recorded
    /// only so an unexpected delivery is visible).
    outputs: Vec<Vec<u8>>,
}

impl CloseHost {
    fn new(answer: c_int) -> CloseHost {
        CloseHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            failures: Vec::new(),
            exits: Vec::new(),
            answer,
            outputs: Vec::new(),
        }
    }
}

impl DriverHost for CloseHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.outputs.push(data.to_vec());
        0
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
        0
    }

    fn failure(&mut self, reason: PortFailure<'_>) -> c_int {
        self.failures.push(match reason {
            PortFailure::Eof => Recorded::Eof,
            PortFailure::Atom(name) => Recorded::Atom(name.to_string()),
            PortFailure::Posix(errno) => Recorded::Posix(errno),
            PortFailure::Error(code) => Recorded::Error(code),
        });
        self.answer
    }

    fn port_exit(&mut self, code: c_int) -> c_int {
        self.exits.push(code);
        self.answer
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers (the idiom of `tests/driver_port_term.rs` / `tests/erl_errno_id.rs`)
// ---------------------------------------------------------------------------------------------

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Run `cmd args…`, returning its stdout on success (`None` when the tool is missing or fails).
fn run_tool(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `true` iff every name in `names` is an **undefined** import of `path`.
fn all_symbols_are_undefined_imports(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--undefined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && line.contains(" UND "))
        }));
    }
    None
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(FIXTURE_NAME, FIXTURE_C)
}

/// Keep the `driver_*`-carrying codegen units in the link: the fixture's undefined imports must
/// resolve against this test binary's dynamic symbol table, and the api table is what publishes
/// them (`#[used]` static + `-Wl,--export-dynamic` from `build.rs`).
fn link_the_families() {
    let table = driver_api_table();
    assert_eq!(
        core::mem::size_of_val(table),
        core::mem::size_of::<beam_abi::driver::api::ErlDrvApi>()
    );
    core::hint::black_box(table);
}

/// The `erl_errno_id` answer for `errno`, as an owned string.
///
/// The answer is NUL-terminated and permanently stored (`src/driver/drv/mem/errno.rs`), so a caller
/// may keep several answers and read them later; this helper copies it out anyway.
fn errno_name(errno: c_int) -> String {
    // SAFETY: `erl_errno_id` accepts any `c_int` and answers a non-null, NUL-terminated string that
    // stays valid for the rest of the process's lifetime (permanent storage).
    let p = unsafe { beam_abi::driver::drv::erl_errno_id(errno) };
    assert!(!p.is_null(), "erl_errno_id always answers a string");
    // SAFETY: non-null and NUL-terminated (the documented contract); read immediately.
    let name = unsafe { CStr::from_ptr(p) };
    name.to_str()
        .expect("the answer is an ASCII POSIX symbol")
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// Half 1: through the C ABI, in this crate
// ---------------------------------------------------------------------------------------------

/// The `driver_*` imports must be undefined in the `.so` (else the C would have bound the calls to
/// something else) — a `SKIP:` when no symbol-table tool exists, a hard failure otherwise.
fn assert_undefined_imports(so: &Path) {
    match all_symbols_are_undefined_imports(so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "{so:?} must leave every fixture import undefined (that is the whole mechanism)"
        ),
        None => skip("no nm/readelf: cannot inspect the fixture's dynamic symbols"),
    }
}

/// Load the fixture and keep the `DriverLibrary` for the caller to drive.
///
/// The `DriverEnv` is deliberately **not** created here: the port handle a driver receives *is* the
/// env's own pointer ([`DriverEnv::as_port`]), so the env must outlive every driver call — the
/// caller constructs it in its own scope (the convention every sibling test follows).
fn load_fixture(path: &str) -> DriverLibrary {
    let lib = load(path, FIXTURE_NAME)
        .expect("a driver whose imports resolve from this test binary must load");
    assert_eq!(lib.name(), FIXTURE_NAME);
    lib
}

#[test]
fn a_real_c_driver_hands_every_spelling_to_one_host_hook() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    assert_undefined_imports(&so);
    let Some(path) = so_path(&so) else { return };

    let mut host = CloseHost::new(0);
    let lib = load_fixture(&path);
    // The `DriverEnv` must outlive every driver call: the `ErlDrvPort` the C stores in `start` is
    // this env's own pointer, and `env_for` recovers the host from it.
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(
        !data.is_null(),
        "start must return the private state, not a failure code"
    );
    // The real C `control` returns the C function's own `int` — the code this crate's host handed
    // back (`0` here) for every valid port, and `-1` for the sentinel.
    for (cmd, expected) in COMMANDS.iter().zip(expected()) {
        let failures_before = host.failures.len();
        let exits_before = host.exits.len();
        let mut reply: *mut c_char = std::ptr::null_mut();
        // SAFETY: `data` is the live state `start` returned; the fixture reads no input buffer and
        // ignores the reply pointer (`rlen == 0` is "length only").
        let n = lib
            .control(data, *cmd, std::ptr::null_mut(), 0, &mut reply, 0)
            .expect("the fixture implements control");
        match &expected {
            Reached::Failure(reason) => {
                assert_eq!(n, 0, "command {cmd}: a valid port must give the C's 0");
                assert_eq!(
                    host.failures.get(failures_before),
                    Some(reason),
                    "command {cmd}: the one hook reached must be `failure` with this reason"
                );
                assert_eq!(
                    host.failures.len(),
                    failures_before + 1,
                    "command {cmd}: exactly one `failure` call"
                );
                assert_eq!(
                    host.exits.len(),
                    exits_before,
                    "command {cmd}: a `driver_failure*` spelling must never reach `port_exit`"
                );
            }
            Reached::Exit(code) => {
                assert_eq!(n, 0, "command {cmd}: a valid port must give the C's 0");
                assert_eq!(
                    host.exits.get(exits_before),
                    Some(code),
                    "command {cmd}: the one hook reached must be `port_exit` with the raw code"
                );
                assert_eq!(
                    host.exits.len(),
                    exits_before + 1,
                    "command {cmd}: exactly one `port_exit` call"
                );
                assert_eq!(
                    host.failures.len(),
                    failures_before,
                    "command {cmd}: `driver_exit` must never reach `failure`"
                );
            }
            Reached::Nothing => {
                assert_eq!(
                    n, -1,
                    "command {cmd}: the invalid-port operand must give the C's -1"
                );
                assert_eq!(
                    host.failures.len(),
                    failures_before,
                    "command {cmd}: an invalid port must reach no `failure` hook"
                );
                assert_eq!(
                    host.exits.len(),
                    exits_before,
                    "command {cmd}: an invalid port must reach no `port_exit` hook"
                );
            }
        }
    }

    // Not vacuous: the seven valid spellings split five `failure` reasons (commands 1-5, with 1 and
    // 5 the same request) and two `port_exit` codes (commands 6-7), and the sentinel reached
    // nothing. Nothing was ever delivered to the port owner by this family.
    assert_eq!(
        host.failures,
        vec![
            Recorded::Eof,
            Recorded::Atom(EXPECTED_ATOM.to_string()),
            Recorded::Posix(libc::EACCES),
            Recorded::Error(7),
            Recorded::Eof,
        ]
    );
    assert_eq!(host.exits, vec![0, libc::EACCES]);
    assert!(
        host.outputs.is_empty(),
        "the closing family delivers nothing through `driver_output`"
    );

    // Tidy up: the driver's `stop` frees the state `start` allocated.
    lib.stop(data).expect("the fixture implements stop");
}

#[test]
fn a_host_without_the_close_hooks_answers_minus_one_for_every_spelling() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    // The trait defaults for `failure`/`port_exit` answer `-1` — the C's failure code. The codes
    // the fixture returns must therefore be the *host's*, not a hardcoded success: with the hooks
    // at their defaults every command but the sentinel reports `-1`, and the sentinel is `-1` too.
    struct DefaultHost {
        heap: beam_alloc::Heap,
        atoms: AtomTable,
    }
    impl DriverHost for DefaultHost {
        fn heap(&mut self) -> &mut beam_alloc::Heap {
            &mut self.heap
        }
        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }
        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            0
        }
        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            0
        }
    }

    let mut host = DefaultHost {
        heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
        atoms: AtomTable::new(),
    };
    let lib = load(&path, FIXTURE_NAME).expect("the fixture loads");
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null());

    for cmd in COMMANDS {
        let mut reply: *mut c_char = std::ptr::null_mut();
        // SAFETY: as above — the fixture reads no input buffer and ignores the reply pointer.
        let n = lib
            .control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
            .expect("the fixture implements control");
        assert_eq!(
            n, -1,
            "command {cmd}: with no host hook the C must report its failure code"
        );
    }

    lib.stop(data).expect("the fixture implements stop");
}

// ---------------------------------------------------------------------------------------------
// Half 2: the same `.so`, driven by the real system ERTS
// ---------------------------------------------------------------------------------------------

/// The Erlang probe: load the driver, open one port, run one command, and print a single stable
/// `ORACLE …` line with everything the C's close makes observable.
///
/// * `Mode` is `"link"` (a `trap_exit` owner: the `{'EXIT',Port,Reason}` the close delivers) or
///   `"monitor"` (`unlink/1` + `monitor(port, …)`: the `{'DOWN',…,Reason}`). The message list
///   reports what actually arrived, so "`driver_exit` unlinks before it closes" is measured, not
///   assumed.
/// * `port_control/3`'s own outcome is reported as the rendered return value, or as
///   `error:badarg` for the negative (`< 0`) control return of the invalid-port sentinel.
/// * The port's fate is reported as `port_info(Port, name)` (`undefined` once closed) plus
///   `port_close/1`'s outcome (`true`, or `error:badarg`).
///
/// One `erl` (and one port) per command: a second `driver_failure_*` on the port a callback just
/// terminated is a measured use-after-free in the C, so it is never exercised.
const ORACLE_MODULE_SRC: &str = r#"%% The oracle probe for the `driver_failure*` / `driver_exit` family, written by the test.
-module(drvfail_probe).
-export([main/1]).

main([Dir, CmdAtom, Mode]) ->
    Cmd = list_to_integer(CmdAtom),
    case erl_ddll:load_driver(Dir, "drvfail") of
        ok -> ok;
        {error, LoadErr} ->
            io:format("LOAD_ERROR ~p~n", [LoadErr]),
            halt(2)
    end,
    case Mode of
        "link" -> process_flag(trap_exit, true);
        "monitor" -> ok
    end,
    Port = open_port({spawn_driver, "drvfail"}, []),
    Ref = case Mode of
              "link" -> undefined;
              "monitor" -> unlink(Port), erlang:monitor(port, Port)
          end,
    Ret = try port_control(Port, Cmd, []) of
              V -> render(V)
          catch Class1:Reason1 -> lists:flatten(io_lib:format("~p:~p", [Class1, Reason1]))
          end,
    Msgs = render(drain(Port, Ref, [])),
    Info = info_label(Port),
    Close = try port_close(Port) of
                V2 -> render(V2)
            catch Class2:Reason2 -> lists:flatten(io_lib:format("~p:~p", [Class2, Reason2]))
            end,
    io:format("ORACLE cmd=~p mode=~s ret=~s msgs=~s info=~s close=~s~n",
              [Cmd, Mode, Ret, Msgs, Info, Close]),
    halt(0).

render(Term) -> lists:flatten(io_lib:format("~p", [Term])).

info_label(Port) ->
    case erlang:port_info(Port, name) of
        undefined -> "undefined";
        {name, Name} -> render({name, Name})
    end.

drain(Port, Ref, Acc) ->
    receive
        {'EXIT', Port, Reason} -> lists:reverse([{exit, Reason} | Acc]);
        {'DOWN', Ref, port, Port, Reason} -> lists:reverse([{down, Reason} | Acc]);
        Other -> drain(Port, Ref, [Other | Acc])
    after 300 -> lists:reverse(Acc)
    end.
"#;

/// The complete `ORACLE …` lines the real ERTS prints for the fixture, **measured** on OTP 29.1.1 /
/// erts-17.1 (`erl -noshell -pa <dir> -run drvfail_probe main <dir> <cmd> <mode>`), in
/// [`COMMANDS`] order for `link` mode and then for `monitor` mode.
const ORACLE_LINES: [&str; 16] = [
    r#"ORACLE cmd=1 mode=link ret=[] msgs=[{exit,normal}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=2 mode=link ret=[] msgs=[{exit,boom}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=3 mode=link ret=[] msgs=[{exit,eacces}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=4 mode=link ret=[] msgs=[{exit,7}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=5 mode=link ret=[] msgs=[{exit,normal}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=6 mode=link ret=[] msgs=[] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=7 mode=link ret=[] msgs=[] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=8 mode=link ret=error:badarg msgs=[] info={name,"drvfail"} close=true"#,
    r#"ORACLE cmd=1 mode=monitor ret=[] msgs=[{down,normal}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=2 mode=monitor ret=[] msgs=[{down,boom}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=3 mode=monitor ret=[] msgs=[{down,eacces}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=4 mode=monitor ret=[] msgs=[{down,7}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=5 mode=monitor ret=[] msgs=[{down,normal}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=6 mode=monitor ret=[] msgs=[{down,normal}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=7 mode=monitor ret=[] msgs=[{down,eacces}] info=undefined close=error:badarg"#,
    r#"ORACLE cmd=8 mode=monitor ret=error:badarg msgs=[] info={name,"drvfail"} close=true"#,
];

/// Run the probe once per command per mode under the real system ERTS, returning the 16 lines (or
/// `None` after a loud skip when a tool is missing or the driver cannot be loaded there).
fn oracle_lines(so: &Path) -> Option<Vec<String>> {
    let dir = so.parent()?.to_path_buf();
    let src = dir.join("drvfail_probe.erl");
    if let Err(err) = std::fs::write(&src, ORACLE_MODULE_SRC) {
        skip(&format!("drvfail oracle: cannot write {src:?}: {err}"));
        return None;
    }
    match Command::new("erlc").arg("-o").arg(&dir).arg(&src).output() {
        Err(err) => {
            skip(&format!("drvfail oracle: cannot run erlc ({err})"));
            return None;
        }
        Ok(out) if !out.status.success() => {
            skip(&format!(
                "drvfail oracle: erlc failed (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Ok(_) => {}
    }

    let mut lines = Vec::with_capacity(ORACLE_LINES.len());
    for mode in ["link", "monitor"] {
        for cmd in COMMANDS {
            // `-run Mod Func Args…`: the function gets the arguments as a list of strings.
            let run = Command::new("erl")
                .arg("-noshell")
                .arg("-pa")
                .arg(&dir)
                .arg("-run")
                .arg("drvfail_probe")
                .arg("main")
                .arg(&dir)
                .arg(cmd.to_string())
                .arg(mode)
                .output();
            let output = match run {
                Err(err) => {
                    skip(&format!("drvfail oracle: cannot run erl ({err})"));
                    return None;
                }
                Ok(out) => out,
            };
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if !output.status.success() {
                skip(&format!(
                    "drvfail oracle: erl failed (exit {:?}) for cmd {cmd}/{mode}:\nstdout: {stdout}\
                     \nstderr: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                ));
                return None;
            }
            if let Some(reason) = stdout.lines().find(|line| line.starts_with("LOAD_ERROR")) {
                skip(&format!(
                    "drvfail oracle: erl_ddll:load_driver/2 refused the fixture ({})",
                    reason.trim()
                ));
                return None;
            }
            let Some(line) = stdout.lines().find(|line| line.starts_with("ORACLE ")) else {
                skip(&format!(
                    "drvfail oracle: no `ORACLE …` line for cmd {cmd}/{mode}:\n{stdout}"
                ));
                return None;
            };
            lines.push(line.trim_end().to_string());
        }
    }
    Some(lines)
}

/// The reason term a [`PortFailure`] must render as, as the probe prints it — the C's
/// `driver_failure_term` reason: `normal` for the eof route, the bare atom for `Atom`/`Posix`, the
/// bare integer for `Error`.
fn oracle_reason(recorded: &Recorded) -> String {
    match recorded {
        Recorded::Eof => "normal".to_string(),
        Recorded::Atom(name) => name.clone(),
        Recorded::Posix(errno) => errno_name(*errno),
        Recorded::Error(code) => code.to_string(),
    }
}

/// The reason term `driver_exit(code)` must render as: `normal` for `0`, else the bare atom
/// `erl_errno_id(code)` names (`io.c:7244-7253`).
fn exit_reason(code: c_int) -> String {
    if code == 0 {
        "normal".to_string()
    } else {
        errno_name(code)
    }
}

#[test]
fn the_oracle_observes_every_driver_failure_reason_and_the_closed_port() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    let Some(lines) = oracle_lines(&so) else {
        return;
    };
    assert_eq!(
        lines.len(),
        ORACLE_LINES.len(),
        "one line per command per mode, got {lines:#?}"
    );

    // (a) Every line is byte-for-byte the value measured on OTP 29.1.1 / erts-17.1.
    for (i, (got, expected)) in lines.iter().zip(ORACLE_LINES.iter()).enumerate() {
        assert_eq!(
            got,
            expected,
            "the real ERTS's probe line {} must equal the pinned value",
            i + 1
        );
    }

    // (b) The reason terms and the port's fate, cross-checked against what *this crate* hands the
    // host: the expected message is derived from the recorded `PortFailure` (through the crate's
    // own `erl_errno_id` for the POSIX spellings), so both runtimes are proven to agree on the term
    // rather than two constants being compared with themselves.
    let expected = expected();
    for (i, reached) in expected.iter().enumerate() {
        let link = &lines[i];
        let monitor = &lines[ORACLE_LINES.len() / 2 + i];
        let cmd = COMMANDS[i];
        let (link_msgs, monitor_msgs) = match reached {
            Reached::Failure(recorded) => {
                let reason = oracle_reason(recorded);
                (
                    format!("msgs=[{{exit,{reason}}}]"),
                    format!("msgs=[{{down,{reason}}}]"),
                )
            }
            // `driver_exit` unlinks the connected process *before* closing, so a link delivers
            // nothing at all while a monitor still observes the close.
            Reached::Exit(code) => ("msgs=[]".to_string(), format!("msgs=[{{down,{}}}]", exit_reason(*code))),
            Reached::Nothing => ("msgs=[]".to_string(), "msgs=[]".to_string()),
        };
        assert!(
            link.contains(&link_msgs),
            "cmd {cmd} (link): the oracle must report {link_msgs:?}: {link:?}"
        );
        assert!(
            monitor.contains(&monitor_msgs),
            "cmd {cmd} (monitor): the oracle must report {monitor_msgs:?}: {monitor:?}"
        );

        // The port's fate: the seven closing spellings leave no port behind (so `port_info/2`
        // answers `undefined` and `port_close/1` raises `badarg`); the invalid-port sentinel
        // refuses the request and leaves the port fully alive.
        let tail = match reached {
            Reached::Nothing => "info={name,\"drvfail\"} close=true",
            _ => "info=undefined close=error:badarg",
        };
        assert!(
            link.contains(tail),
            "cmd {cmd} (link): the port's fate must be {tail:?}: {link:?}"
        );
        assert!(
            monitor.contains(tail),
            "cmd {cmd} (monitor): the port's fate must be {tail:?}: {monitor:?}"
        );
    }

    // Not vacuous: five link lines carry a port exit (commands 1-5) and three carry none (the two
    // `driver_exit` spellings, which unlink first, and the sentinel); the sentinel keeps its port,
    // which is the only `info={name,…}` line in either mode.
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("msgs=[{exit,"))
            .count(),
        5
    );
    assert_eq!(
        lines.iter().filter(|line| line.contains("info={name,")).count(),
        2
    );
    assert_eq!(
        lines.iter().filter(|line| line.contains("close=true")).count(),
        2
    );
    assert!(
        lines.iter().all(|line| !line.contains("timeout")),
        "every probe collected its messages within the budget"
    );
}
