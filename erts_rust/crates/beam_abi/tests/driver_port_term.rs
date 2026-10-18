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

//! End-to-end proof of the **`ERL_DRV_PORT` term-data operand** of the driver C ABI (workspace
//! contract **A.4**): the real C driver `tests/fixtures/driver_port_term.c` — compiled with the
//! system `cc` against the system `erl_driver.h` — is `dlopen`ed, validated and driven through the
//! Rust loader, and the very same `.so` is then loaded and driven by the **real system ERTS**.
//!
//! # The gap this pins
//!
//! The C idiom for putting a port in a term-data array is
//! `{ERL_DRV_PORT, driver_mk_port(port)}`: in the emulator `driver_mk_port` answers the port's **id
//! term** (`prt->common.id`, `io.c:7295-7303`), which the decoder's `ERL_DRV_PORT` case pushes
//! as-is (`mess = ptr[0]`, `io.c:5969-5972`) after `is_internal_port` (`io.c:5744-5747`). This
//! crate's [`beam_abi::driver::drv::driver_mk_port`] has no port table, so it answers the **handle**
//! encoding (a port handle *is* the `DriverEnv` pointer). The operand is resolved back to the port's
//! id term by the defaulted [`beam_abi::DriverHost::port_term`] hook, which
//! [`beam_abi::driver::host::decode_term_spec`] consults when the operand is not a port term
//! already — so the driver written the C way delivers the port term, exactly as the emulator does.
//!
//! # Two halves, both measured
//!
//! * [`a_real_c_driver_resolves_a_port_handle_operand_to_the_port_term`] drives the `.so` through
//!   this crate's driver loader with a recording [`beam_abi::DriverHost`] whose `port_term` answers
//!   a synthesized internal port term, and asserts the delivered message is **exactly** that term
//!   for both `erl_drv_output_term` and `erl_drv_send_term`, that the hook was asked about the
//!   driver's own handle, and that a genuinely malformed operand is still refused.
//! * [`the_oracle_delivers_the_same_port_term_from_real_erts`] loads and drives the *same* `.so`
//!   under the system `erl` (`erl_ddll:load_driver/2` + `open_port({spawn_driver, …})` +
//!   `port_control/3`) and asserts the printed report equals the value recorded in
//!   [`ORACLE_LINES`] — measured on OTP 29.1.1 / erts-17.1 — and that the delivered-message shape
//!   is the one this crate produces.
//!
//! The only difference between the two runtimes' raw codes is the *success* value: the C answers
//! `1` ("delivered"), this crate's host seam answers `0` (a documented deviation whose `< 0`
//! failure predicate is identical), so both sides are compared through their own recorded codes plus
//! the shared delivered/failed verdict.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`); the oracle half additionally needs `erlc`/`erl`. When a facility is missing the
//! affected test prints a loud `SKIP:` banner and returns — never a silent pass. When the toolchain
//! **is** present every assertion is hard.

mod common;

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::load;
use beam_abi::driver::types::ErlDrvPort;
use beam_abi::{DriverEnv, DriverHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, is_atom, is_tuple, make_atom, make_internal_pid, make_internal_port, make_pid_data,
    make_port_data, tuple_element, tuple_size, Eterm,
};
use libc::{c_char, c_int, c_uint};
use common::{compile_so, skip, TestDriverHost};

/// The committed C fixture — a real driver (see the file header for the A.4 rationale).
const FIXTURE_C: &str = include_str!("fixtures/driver_port_term.c");

/// The entry's `driver_name`, which is also the `.so` name: `erl_ddll:load_driver/2` and
/// [`load`] both reject a library whose `driver_name` differs from the requested name
/// (`bad_driver_name`), so the scratch `.so` is compiled with this stem.
const FIXTURE_NAME: &str = "portterm";

/// The `driver_*` / `erl_drv_*` symbols the fixture imports. They must be *undefined* in the `.so`
/// and *defined* in the loading process (`build.rs`'s `-Wl,--export-dynamic` + the `#[used]` api
/// table), so a successful `dlopen(RTLD_NOW)` is itself evidence the family is exported.
const FIXTURE_IMPORTS: [&str; 8] = [
    "driver_alloc",
    "driver_free",
    "driver_output",
    "driver_mk_atom",
    "driver_mk_port",
    "driver_connected",
    "erl_drv_output_term",
    "erl_drv_send_term",
];

/// The four probe commands, in the order both halves drive them (`PT_CMD_*` in the fixture).
const COMMANDS: [c_uint; 4] = [1, 2, 3, 4];

/// The reports this crate produces, per command (`<label>=<host code>`, `0` = delivered).
const RUST_REPORTS: [&str; 4] = ["output=0", "send=0", "bad=-1", "tuple=0"];

/// The reports the **real system ERTS** (OTP 29.1.1 / erts-17.1) prints for the same commands,
/// measured by [`the_oracle_delivers_the_same_port_term_from_real_erts`]: the C's success code is
/// `1` ("the message was delivered", `driver_deliver_term`) and a rejected spec is `-1`.
const ORACLE_REPORTS: [&str; 4] = ["output=1", "send=1", "bad=-1", "tuple=1"];

/// The delivered messages' classification, as both runtimes must agree on it (`port_self` = the
/// port's own id term, `tagged_port_self` = `{tag, Port}`, none = the spec was refused).
const EXPECTED_TERMS: [&[&str]; 4] = [&["port_self"], &["port_self"], &[], &["tagged_port_self"]];

/// The complete `ORACLE …` lines the real ERTS prints, **measured** on OTP 29.1.1 / erts-17.1.
const ORACLE_LINES: [&str; 4] = [
    "ORACLE cmd=1 terms=[port_self] report=output=1",
    "ORACLE cmd=2 terms=[port_self] report=send=1",
    "ORACLE cmd=3 terms=[] report=bad=-1",
    "ORACLE cmd=4 terms=[tagged_port_self] report=tuple=1",
];

// ---------------------------------------------------------------------------------------------
// Helpers (the idiom of `tests/driver_binary_refc.rs`)
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

/// The shared delivered/failed verdict of a report: the C's own `< 0` idiom, which is identical on
/// both runtimes even though their *success* code differs (`1` in the C, `0` here).
fn delivered(report: &str) -> bool {
    !report.ends_with("=-1")
}

/// Classify one delivered message the way the oracle probe prints it (`port_self`,
/// `tagged_port_self`) — `None` for anything unexpected.
fn classify(term: Eterm, port_term: Eterm, atoms: &AtomTable) -> Option<&'static str> {
    if term == port_term {
        return Some("port_self");
    }
    // SAFETY: the readers below run on a live term the decoder built; `is_tuple` gates the tuple
    // accessors and `tuple_size` is read before any element.
    if unsafe { is_tuple(term) } && unsafe { tuple_size(term) } == 2 {
        // SAFETY: `term` is a live 2-tuple (checked above).
        let (first, second) = unsafe { (tuple_element(term, 0), tuple_element(term, 1)) };
        let tagged = is_atom(first) && atoms.name(atom_val(first)).as_deref() == Some("tag");
        if tagged && second == port_term {
            return Some("tagged_port_self");
        }
    }
    None
}
// ---------------------------------------------------------------------------------------------
// Half 1: through the C ABI, in this crate
// ---------------------------------------------------------------------------------------------

#[test]
fn a_real_c_driver_resolves_a_port_handle_operand_to_the_port_term() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "{:?} must leave every fixture import undefined (that is the whole mechanism)",
            so
        ),
        None => skip("no nm/readelf: cannot inspect the fixture's dynamic symbols"),
    }
    let Some(path) = so_path(&so) else { return };

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ---------------------------------------
    let lib = load(&path, FIXTURE_NAME)
        .expect("a driver whose imports resolve from this test binary must load");
    assert_eq!(lib.name(), FIXTURE_NAME);

    // The port id term the host answers — a synthesized internal port term, the shape
    // `driver_mk_port` returns in the C. The connected pid is the `erl_drv_send_term` destination.
    let port_term = make_internal_port(make_port_data(9));
    let pid = make_internal_pid(make_pid_data(1, 42));

    let mut host = TestDriverHost::new();
    host.set_port_term(Some(port_term));
    host.set_connected(Some(pid));

    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null(), "start must return the private state, not a failure code");

    // Drive the four commands one at a time. Nothing borrows `host` across a driver call — the
    // recording host sits behind `DriverEnv`'s raw pointer for the duration — so `host` is read
    // strictly before and after each call.
    let mut reports: Vec<String> = Vec::new();
    let mut delivered: Vec<Vec<(Option<Eterm>, Eterm)>> = Vec::new();
    for cmd in COMMANDS {
        let outputs_before = host.outputs().len();
        let sent_before = host.sent().len();
        let mut reply: *mut c_char = std::ptr::null_mut();
        // SAFETY: `data` is the live state `start` returned; the fixture reads no input buffer; a
        // null reply pointer with `rlen == 0` is the "length only" shape `port_control/3` uses.
        let n = lib
            .control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
            .expect("the fixture implements control");
        assert_eq!(
            n, 0,
            "the fixture answers through driver_output, never the (truncating) control reply buffer"
        );
        let outputs = host.outputs();
        assert_eq!(
            outputs.len(),
            outputs_before + 1,
            "control command {cmd} must publish exactly one report through driver_output"
        );
        reports.push(
            String::from_utf8(outputs.last().expect("just checked").clone())
                .expect("the report is ASCII"),
        );
        delivered.push(host.sent()[sent_before..].to_vec());
    }

    // The raw codes: this crate's host answers `0` for "delivered" and `-1` for a refused spec (the
    // C answers `1`/`-1`; the `< 0` predicate is the same).
    assert_eq!(reports, RUST_REPORTS);
    assert_eq!(host.outputs().len(), COMMANDS.len(), "no stray output");

    // --- command 1: `erl_drv_output_term` with an `ERL_DRV_PORT` operand -----------------------
    assert_eq!(delivered[0].len(), 1, "exactly one term must be delivered");
    assert_eq!(delivered[0][0].0, None, "the `_output_term` form addresses the port owner");
    assert_eq!(delivered[0][0].1, port_term, "the delivered term IS the port's id term");
    assert_eq!(
        classify(delivered[0][0].1, port_term, host.atoms()),
        Some(EXPECTED_TERMS[0][0]),
        "the delivered message classifies as the oracle's"
    );

    // --- command 2: `erl_drv_send_term` with an `ERL_DRV_PORT` operand -------------------------
    assert_eq!(delivered[1].len(), 1);
    assert_eq!(
        delivered[1][0].0,
        Some(pid),
        "the `_send_term` form addresses its `to` operand"
    );
    assert_eq!(delivered[1][0].1, port_term, "the sent term IS the port's id term");
    assert_eq!(
        classify(delivered[1][0].1, port_term, host.atoms()),
        Some(EXPECTED_TERMS[1][0])
    );

    // --- command 3: a genuinely malformed operand --------------------------------------------
    // `1` is neither a port term (low nibble 1, not the port tag 7) nor a live handle, so the spec
    // is refused: the hook is consulted, answers `None`, and nothing is delivered.
    assert!(delivered[2].is_empty(), "a refused spec delivers nothing");

    // --- command 4: the port term nested in a tuple ------------------------------------------
    assert_eq!(delivered[3].len(), 1, "the tuple is one delivered message");
    assert_eq!(delivered[3][0].0, None);
    assert_eq!(
        classify(delivered[3][0].1, port_term, host.atoms()),
        Some(EXPECTED_TERMS[3][0])
    );

    // Every `ERL_DRV_PORT` operand is offered to the hook: the driver's own handle for commands
    // 1, 2 and 4, and the malformed `1` of command 3 — which the hook refuses.
    assert_eq!(
        host.port_handles(),
        &[port, port, 1usize as ErlDrvPort, port],
        "the hook is asked about exactly one handle per port operand"
    );
}

/// A [`DriverHost`] that implements **nothing beyond the required surface** — in particular it does
/// not override [`DriverHost::port_term`], so it answers the trait default (`None`).
///
/// This is the "unchanged behaviour" half of the pin: a host that cannot resolve a handle refuses
/// the operand (`-1`), which is exactly what this crate did before the hook existed.
struct NoPortTermHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
    outputs: Vec<Vec<u8>>,
    sent: Vec<Eterm>,
}

impl NoPortTermHost {
    fn new() -> NoPortTermHost {
        NoPortTermHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            outputs: Vec::new(),
            sent: Vec::new(),
        }
    }
}

impl DriverHost for NoPortTermHost {
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

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, term: Eterm) -> c_int {
        self.sent.push(term);
        0
    }
}

#[test]
fn a_host_without_the_port_term_hook_still_refuses_the_handle_operand() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    let Some(path) = so_path(&so) else { return };
    let lib = load(&path, FIXTURE_NAME).expect("the fixture loads");

    let mut host = NoPortTermHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");

    // Commands 1, 2 and 4 all carry a handle operand: with the hook at its default they are
    // refused (`-1`), and nothing is delivered — the pre-hook behaviour, preserved.
    for (i, cmd) in [COMMANDS[0], COMMANDS[1], COMMANDS[3]].into_iter().enumerate() {
        let mut reply: *mut c_char = std::ptr::null_mut();
        // SAFETY: as `drive` — the fixture reads no input buffer and ignores the reply pointer.
        let n = lib
            .control(data, cmd, std::ptr::null_mut(), 0, &mut reply, 0)
            .expect("the fixture implements control");
        assert_eq!(n, 0);
        let report = String::from_utf8(host.outputs[i].clone()).expect("the report is ASCII");
        assert!(
            report.ends_with("=-1"),
            "command {cmd} must be refused without the hook, got {report:?}"
        );
    }
    assert!(host.sent.is_empty(), "a refused spec never delivers a term");
}

// ---------------------------------------------------------------------------------------------
// Half 2: the same `.so`, driven by the real system ERTS
// ---------------------------------------------------------------------------------------------

/// The Erlang probe: load the driver, open a port, and drive the four commands, printing one
/// `ORACLE …` line per command (the same vocabulary [`classify`] uses on the Rust side).
///
/// `run/2` drains the mailbox after each `port_control/3`: the report `driver_output` publishes
/// arrives as `{Port,{data,Data}}` and terminates the drain, while a delivered term arrives as the
/// message itself and is classified.
const ORACLE_MODULE_SRC: &str = r#"%% The oracle probe for the `ERL_DRV_PORT` term-data operand, written by the test.
-module(portterm_probe).
-export([main/1]).

main([Dir]) ->
    case erl_ddll:load_driver(Dir, "portterm") of
        ok -> ok;
        {error, Reason} ->
            io:format("LOAD_ERROR ~p~n", [Reason]),
            halt(2)
    end,
    Port = open_port({spawn_driver, "portterm"}, []),
    [run(Port, Cmd) || Cmd <- [1, 2, 3, 4]],
    halt(0).

run(Port, Cmd) ->
    _ = port_control(Port, Cmd, []),
    {Terms, Report} = drain(Port, []),
    io:format("ORACLE cmd=~p terms=~p report=~s~n", [Cmd, classify(Terms, Port), Report]).

drain(Port, Acc) ->
    receive
        {Port, {data, Data}} -> {lists:reverse(Acc), Data};
        Other -> drain(Port, [Other | Acc])
    after 2000 -> {lists:reverse(Acc), "timeout"}
    end.

classify(Terms, Port) ->
    [classify_one(T, Port) || T <- Terms].

classify_one(T, Port) when is_port(T), T =:= Port -> port_self;
classify_one(T, Port) when is_tuple(T), tuple_size(T) =:= 2,
                           element(1, T) =:= tag, element(2, T) =:= Port -> tagged_port_self;
classify_one(T, _Port) -> {unexpected, T}.
"#;

/// Run the probe under the real system ERTS; `None` (after a loud skip) when a tool is missing or
/// the driver cannot be loaded there.
fn oracle_lines(so: &Path) -> Option<Vec<String>> {
    let dir = so.parent()?.to_path_buf();
    let src = dir.join("portterm_probe.erl");
    if let Err(err) = std::fs::write(&src, ORACLE_MODULE_SRC) {
        skip(&format!("portterm oracle: cannot write {src:?}: {err}"));
        return None;
    }
    match Command::new("erlc").arg("-o").arg(&dir).arg(&src).output() {
        Err(err) => {
            skip(&format!("portterm oracle: cannot run erlc ({err})"));
            return None;
        }
        Ok(out) if !out.status.success() => {
            skip(&format!(
                "portterm oracle: erlc failed (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Ok(_) => {}
    }

    // `-run Mod Func Arg`: the function gets the arguments as a list of strings.
    let run = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-run")
        .arg("portterm_probe")
        .arg("main")
        .arg(&dir)
        .output();
    let output = match run {
        Err(err) => {
            skip(&format!("portterm oracle: cannot run erl ({err})"));
            return None;
        }
        Ok(out) => out,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        skip(&format!(
            "portterm oracle: erl failed (exit {:?}):\nstdout: {stdout}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    if let Some(reason) = stdout.lines().find(|line| line.starts_with("LOAD_ERROR")) {
        skip(&format!(
            "portterm oracle: erl_ddll:load_driver/2 refused the fixture ({})",
            reason.trim()
        ));
        return None;
    }
    let lines: Vec<String> = stdout
        .lines()
        .filter(|line| line.starts_with("ORACLE "))
        .map(|line| line.trim_end().to_string())
        .collect();
    if lines.len() != COMMANDS.len() {
        skip(&format!(
            "portterm oracle: expected {} `ORACLE …` lines, got {}:\n{stdout}",
            COMMANDS.len(),
            lines.len()
        ));
        return None;
    }
    Some(lines)
}

#[test]
fn the_oracle_delivers_the_same_port_term_from_real_erts() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    let Some(lines) = oracle_lines(&so) else { return };

    // The measured values this crate pins, line by line.
    for (i, (got, expected)) in lines.iter().zip(ORACLE_LINES.iter()).enumerate() {
        assert_eq!(got, expected, "the real ERTS's probe line {} must equal the pinned value", i + 1);
    }

    // And the cross-runtime claim: the delivered-message shape the real ERTS reports is the one
    // this crate's decoder produces for the same `.so`. The per-command reports differ only in the
    // success code (`1` in the C, `0` here), so they are compared through the shared verdict.
    for ((line, expected_terms), expected_report) in lines
        .iter()
        .zip(EXPECTED_TERMS.iter())
        .zip(ORACLE_REPORTS.iter())
    {
        let rendered = format!("terms=[{}]", expected_terms.join(","));
        assert!(
            line.contains(&rendered),
            "the oracle line {line:?} must report the delivered terms this crate produces ({rendered})"
        );
        let report = line
            .split_once("report=")
            .map(|(_, report)| report)
            .expect("every probe line carries its report");
        assert_eq!(report, *expected_report);
    }
    // The two runtimes' raw codes agree on what was delivered and what was refused.
    for (rust, oracle) in RUST_REPORTS.iter().zip(ORACLE_REPORTS.iter()) {
        assert_eq!(
            delivered(rust),
            delivered(oracle),
            "the delivered/failed verdict must agree: rust {rust:?} vs oracle {oracle:?}"
        );
    }
    // Not vacuous: command 3 is the refusal on both sides, and it is the only one.
    assert_eq!(
        RUST_REPORTS.iter().filter(|report| !delivered(report)).count(),
        1
    );
}
