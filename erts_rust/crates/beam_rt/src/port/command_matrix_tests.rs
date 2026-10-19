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
//! The `erlang:port_command/2,3` **row matrix**: what the BIF answers for an **idle** port (both
//! runtimes reachable), and what it does for a **busy** one (this runtime only — see below).
//!
//! # The C's decision order
//!
//! `erts_internal_port_command_3` (`erts/emulator/beam/erl_bif_port.c:168-236`) decides in a fixed
//! order, and every row below is one cell of it:
//!
//! 1. every `/3` option element is validated first — an unknown atom, a non-atom element or an
//!    improper tail is `badarg` *before* the port is even looked up (`:175-190`);
//! 2. the port is resolved (`sig_lookup_port`) — a dead or unknown port is `badarg` (`:191-193`);
//! 3. `[force]` on a driver whose **entry** does not advertise `ERL_DRV_FLAG_SOFT_BUSY` is
//!    `error:notsup` (`:195-198`) — a fact about the entry, so it holds on an idle port too;
//! 4. only then does `erts_port_output` run, and *it* checks the port's busy state **before** it
//!    converts the iolist (`erts/emulator/beam/io.c:1749-1754` against the conversion at `:1790`):
//!    a busy port is `ERTS_PORT_OP_BUSY`, which the BIF turns into the atom `false` under
//!    `[nosuspend]` (`:211-214`) and into a caller **suspend** + a re-issued BIF otherwise
//!    (`:215-220`).
//!
//! Step 4's ordering is the subtle one: a busy port's `[nosuspend]` answers `false` even for a
//! `Data` that is **not iodata at all**, because the command is dropped before the conversion ever
//! looks at it. [`the_busy_dropped_rows_write_nothing`] pins exactly that.
//!
//! # What the oracle can and cannot reach
//!
//! The **idle** rows are open to both runtimes: a `{spawn_executable, "/bin/sh"}` port (whose
//! `spawn_driver` entry has no `ERL_DRV_FLAG_SOFT_BUSY`, so `[force]` is `notsup`) and a
//! `{spawn_driver, "ram_file_drv"}` port (whose entry declares no flags at all). Their whole
//! session is compared character for character with the live `erl` — with the primary logger level
//! pinned to `none` first, because this runtime's C answer to a non-iodata `Data` is a *raise*
//! while the oracle's additionally logs a timestamped `Bad value on output port '<name>'` report
//! (`bad_sig_schedule`, `io.c:1274-1277`) that would otherwise race into the compared stdout.
//!
//! The **busy** rows have **no oracle-reachable fixture**: no driver shipped with OTP is both
//! loaded and busy on demand (a busy driver has to call `set_busy_port` from inside its own
//! callback), so `[force]`/`[nosuspend]` on a busy port cannot be measured against `erl` at all.
//! They are driven here through [`super::test_support::register_busy_port`] — a registered driver
//! whose busy state and `SOFT_BUSY` claim the test controls, and whose `output` callback counts
//! its own invocations — so each dropped or refused arm can assert that the command reached **no
//! write**: the driver's `output` never ran and `port_info(Port, output)` stayed `0`. Nothing in
//! those rows is compared with the oracle; they are checked against the C above, arm by arm.
use super::tests::{raised_text, runtime_list, runtime_tuple};
use super::*;
use crate::native::test_support::{atom, call, chars, runtime as native_runtime};
use super::test_support::register_busy_port;
use crate::runtime::Runtime;
/// The port a row drives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Port {
    /// `{spawn_executable, "/bin/sh"}` with `{args, ["-c", "cat >/dev/null"]}` — a `spawn_driver`
    /// port whose entry carries no `ERL_DRV_FLAG_SOFT_BUSY`.
    Shell,
    /// `{spawn_driver, "ram_file_drv"}` — a driver with neither a `control` callback nor any
    /// declared entry flags.
    RamFile,
}
/// A row's `Data` operand.
#[derive(Clone, Copy)]
enum Data {
    /// A char-list payload (`port_command(P, "x")`).
    Text(&'static str),
    /// A binary payload.
    Bytes(&'static [u8]),
    /// A bare small integer — **not** iodata (`port_command(P, 42)`).
    Small(i64),
    /// A one-element list of integers — iodata (`port_command(P, [42])`).
    IntList(&'static [i64]),
}
/// A recognized `/3` option (`force` / `nosuspend`).
#[derive(Clone, Copy)]
enum CmdOpt {
    /// The atom `force`.
    Force,
    /// The atom `nosuspend`.
    NoSuspend,
}
/// One matrix row.
struct Row {
    /// The label both sides print.
    label: &'static str,
    /// The port the row drives.
    port: Port,
    /// The `Data` operand.
    data: Data,
    /// The `/3` option list (`[]` means the `/2` arity — the oracle's own equivalence).
    options: &'static [CmdOpt],
}
/// The `ram_file_drv` `OPEN` command — `flags:i32 = 3` followed by the initial content `"abc"` —
/// used to bring that port into a state where a following (`FSYNC`) command is well-formed.
const OPEN_ABC: &[u8] = &[1, 0, 0, 0, 3, 97, 98, 99];
/// The `ram_file_drv` `FSYNC` command: a well-formed one-byte command whose reply is irrelevant to
/// the row (a `port_command` answer is the *acceptance*, not the reply).
const FSYNC: &[u8] = &[9];
/// The matrix, in table order.
const ROWS: &[Row] = &[
    Row {
        label: "shell_plain",
        port: Port::Shell,
        data: Data::Text("x"),
        options: &[],
    },
    Row {
        label: "shell_nosuspend",
        port: Port::Shell,
        data: Data::Text("x"),
        options: &[CmdOpt::NoSuspend],
    },
    Row {
        label: "shell_force",
        port: Port::Shell,
        data: Data::Text("x"),
        options: &[CmdOpt::Force],
    },
    Row {
        label: "shell_force_nosuspend",
        port: Port::Shell,
        data: Data::Text("x"),
        options: &[CmdOpt::Force, CmdOpt::NoSuspend],
    },
    Row {
        label: "shell_non_iodata",
        port: Port::Shell,
        data: Data::Small(42),
        options: &[],
    },
    Row {
        label: "shell_iodata_integer_list",
        port: Port::Shell,
        data: Data::IntList(&[42]),
        options: &[],
    },
    Row {
        label: "ram_file_plain",
        port: Port::RamFile,
        data: Data::Bytes(FSYNC),
        options: &[],
    },
    Row {
        label: "ram_file_nosuspend",
        port: Port::RamFile,
        data: Data::Bytes(FSYNC),
        options: &[CmdOpt::NoSuspend],
    },
    Row {
        label: "ram_file_force",
        port: Port::RamFile,
        data: Data::Bytes(FSYNC),
        options: &[CmdOpt::Force],
    },
    Row {
        label: "ram_file_force_nosuspend",
        port: Port::RamFile,
        data: Data::Bytes(FSYNC),
        options: &[CmdOpt::Force, CmdOpt::NoSuspend],
    },
];
/// `bytes` as the Erlang binary literal both sides are written with: `<<1,0,0,0,3,97,98,99>>`.
fn erl_binary(bytes: &[u8]) -> String {
    let rendered = bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{rendered}>>")
}
/// A row's `Data` as an Erlang source expression.
fn oracle_data(data: Data) -> String {
    match data {
        Data::Text(text) => format!("{text:?}"),
        Data::Bytes(bytes) => erl_binary(bytes),
        Data::Small(value) => value.to_string(),
        Data::IntList(values) => {
            let rendered = values.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            format!("[{rendered}]")
        }
    }
}
/// A row's option list as an Erlang source expression.
fn oracle_options(options: &[CmdOpt]) -> String {
    let rendered = options
        .iter()
        .map(|option| match option {
            CmdOpt::Force => "force",
            CmdOpt::NoSuspend => "nosuspend",
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{rendered}]")
}
/// The oracle's whole session.
///
/// Both ports are opened first (the `ram_file_drv` one is brought to `OPEN` state and its reply
/// drained), then every row is issued and rendered as `label=<value|error:Reason>`.
///
/// The primary logger level is pinned to `none` because the non-iodata row makes the **oracle**
/// log a timestamped `Bad value on output port '/bin/sh'` report through `bad_sig_schedule`
/// (`io.c:1274-1277`) on standard_io, asynchronously — a report that would land in the compared
/// stdout at a time the probe cannot predict.
fn oracle_expression() -> String {
    let rows = ROWS
        .iter()
        .map(|row| {
            let port = match row.port {
                Port::Shell => "Sh",
                Port::RamFile => "Ram",
            };
            format!(
                "{{{:?},{port},{},{}}}",
                row.label,
                oracle_data(row.data),
                oracle_options(row.options)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "logger:set_primary_config(level, none), \
         Sh = open_port({{spawn_executable, \"/bin/sh\"}}, \
                        [{{args, [\"-c\", \"cat >/dev/null\"]}}]), \
         Ram = open_port({{spawn_driver, \"ram_file_drv\"}}, []), \
         true = port_command(Ram, {open}), \
         receive {{Ram, {{data, _}}}} -> ok after 2000 -> erlang:error(no_open_reply) end, \
         F = fun({{L, P, D, O}}) -> \
               C = case O of \
                     [] -> fun() -> port_command(P, D) end; \
                     _ -> fun() -> port_command(P, D, O) end \
                   end, \
               R = try C() of V -> lists:flatten(io_lib:format(\"~p\", [V])) \
                   catch error: E -> \"error:\" ++ lists:flatten(io_lib:format(\"~p\", [E])) \
                   end, \
               L ++ \"=\" ++ R \
             end, \
         io:format(\"~s\", [lists:join(\",\", [F(R) || R <- [{rows}]])]).",
        open = erl_binary(OPEN_ABC)
    )
}
/// `{elements…}` as a tuple on the runtime's own heap.
fn tuple_of(runtime: &mut Runtime, elements: &[Eterm]) -> Eterm {
    runtime_tuple(runtime, elements)
}
/// A row's `Data` as this runtime's own term.
fn runtime_data(runtime: &mut Runtime, data: Data) -> Eterm {
    match data {
        Data::Text(text) => chars(runtime, text),
        // `OPEN_ABC`/`FSYNC` are far below the on-heap binary limit, so this really is a single
        // heap binary (the same shape the oracle's `<<…>>` literal has).
        Data::Bytes(bytes) => {
            terms::make_heap_binary(&mut RuntimeContext::new(runtime), bytes).expect("a binary")
        }
        Data::Small(value) => make_small(value),
        Data::IntList(values) => {
            let elements = values.iter().copied().map(make_small).collect::<Vec<_>>();
            runtime_list(runtime, &elements)
        }
    }
}
/// A row's option list as this runtime's own term.
fn runtime_options(runtime: &mut Runtime, options: &[CmdOpt]) -> Eterm {
    let elements = options
        .iter()
        .map(|option| match option {
            CmdOpt::Force => atom(runtime, "force"),
            CmdOpt::NoSuspend => atom(runtime, "nosuspend"),
        })
        .collect::<Vec<_>>();
    runtime_list(runtime, &elements)
}
/// The two ports a session drives, opened through the runtime's own `open_port/2`.
struct Session {
    /// The `beam_rt` runtime every row is dispatched through.
    runtime: Runtime,
    /// The `{spawn_executable, "/bin/sh"}` port.
    shell: Eterm,
    /// The `{spawn_driver, "ram_file_drv"}` port, already in `OPEN` state.
    ram_file: Eterm,
}
impl Session {
    /// Opens both ports and brings the `ram_file_drv` one to `OPEN` state.
    ///
    /// The `OPEN` command is issued exactly as the oracle's own setup issues it, so a row's
    /// `FSYNC` sees the same driver state on both sides.
    fn open() -> Session {
        let mut runtime = native_runtime();
        let executable = atom(&mut runtime, "spawn_executable");
        let path = chars(&mut runtime, "/bin/sh");
        let name = tuple_of(&mut runtime, &[executable, path]);
        let tag = atom(&mut runtime, "args");
        let dash_c = chars(&mut runtime, "-c");
        let command = chars(&mut runtime, "cat >/dev/null");
        let arguments = runtime_list(&mut runtime, &[dash_c, command]);
        let args = tuple_of(&mut runtime, &[tag, arguments]);
        let options = runtime_list(&mut runtime, &[args]);
        let shell = call(&mut runtime, "erlang", "open_port", &[name, options])
            .expect("open_port({spawn_executable, \"/bin/sh\"}, [{args, …}])");
        let driver = atom(&mut runtime, "spawn_driver");
        let driver_name = chars(&mut runtime, "ram_file_drv");
        let name = tuple_of(&mut runtime, &[driver, driver_name]);
        let ram_file = call(&mut runtime, "erlang", "open_port", &[name, NIL])
            .expect("open_port({spawn_driver, \"ram_file_drv\"}, [])");
        let open = runtime_data(&mut runtime, Data::Bytes(OPEN_ABC));
        let answer = call(&mut runtime, "erlang", "port_command", &[ram_file, open])
            .expect("the OPEN command is accepted");
        assert_eq!(
            runtime.atom_name_of(answer),
            Some("true"),
            "`port_command(Ram, Open)` answers `true`"
        );
        Session {
            runtime,
            shell,
            ram_file,
        }
    }
    /// One row, rendered exactly as [`oracle_expression`] renders it: `label=<value|error:Reason>`.
    fn row(&mut self, row: &Row) -> String {
        let port = match row.port {
            Port::Shell => self.shell,
            Port::RamFile => self.ram_file,
        };
        let data = runtime_data(&mut self.runtime, row.data);
        // An empty option list is the `/2` arity — the oracle's own equivalence, which the
        // expression above reproduces (it only appends `[Options]` when the row has any).
        let mut args = vec![port, data];
        if !row.options.is_empty() {
            args.push(runtime_options(&mut self.runtime, row.options));
        }
        match call(&mut self.runtime, "erlang", "port_command", &args) {
            Ok(answer) => format!(
                "{}={}",
                row.label,
                self.runtime
                    .atom_name_of(answer)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{answer:?}"))
            ),
            Err(error) => format!("{}={}", row.label, raised_text(&error)),
        }
    }
    /// The whole matrix, comma-joined — the exact string [`oracle_expression`] prints.
    fn session(&mut self) -> String {
        ROWS.iter()
            .map(|row| self.row(row))
            .collect::<Vec<_>>()
            .join(",")
    }
}
/// The idle-port matrix agrees with the live oracle, row for row and in table order.
///
/// Three facts are pinned by the same comparison: `[force]` is `error:notsup` on **both** kinds of
/// idle port (the entry-flag test, not a state test), `[nosuspend]` on an idle port changes nothing
/// (`true`), and a non-iodata `Data` on an idle port is `error:badarg` while `[42]` really is
/// iodata (`true`).
#[test]
fn the_idle_port_rows_agree_with_the_oracle() {
    let observed = Session::open().session();
    match super::ram_file_matrix_tests::oracle_output(&oracle_expression()) {
        Some(oracle) => assert_eq!(
            observed, oracle,
            "`erlang:port_command/2,3` must answer like the oracle for every idle-port row"
        ),
        None => super::tests::skip_oracle_check("the `port_command/2,3` idle-port row matrix"),
    }
}
/// The rows on this runtime **alone** — the assertion that holds with no oracle on `PATH`.
///
/// `[force]` is `notsup` (never `badarg`, never a capability gap: a `notsup` raise does not start
/// with `unsupported`), `[nosuspend]` on an idle port is a no-op, and the non-iodata `Data` is
/// `badarg` — the conversion really does run on an idle port.
#[test]
fn the_idle_port_rows_hold_without_the_oracle() {
    let mut session = Session::open();
    for row in ROWS {
        let rendered = session.row(row);
        let (label, answer) = rendered
            .split_once('=')
            .unwrap_or_else(|| panic!("a rendered row: {rendered}"));
        let expected = match (row.label, row.options) {
            (_, [CmdOpt::Force, ..]) => "error:notsup",
            ("shell_non_iodata", _) => "error:badarg",
            _ => "true",
        };
        assert_eq!(answer, expected, "{label}: {rendered}");
    }
}
/// The port's own `port_info(Port, output)` byte counter — the runtime's written-bytes figure for
/// `port`, read through the BIF the oracle exposes.
fn output_counter(runtime: &mut Runtime, port: Eterm) -> i64 {
    let item = atom(runtime, "output");
    let answer = call(runtime, "erlang", "port_info", &[port, item]).expect("port_info/2");
    let value = terms::tuple_element(answer, 1).expect("the {output, N} pair");
    signed_value(value)
}
/// The dropped and refused arms of a **busy** port write nothing: the command never reaches the
/// driver's `output` callback and `port_info(Port, output)` stays `0`.
///
/// Four arms are pinned, in the C's order (`erl_bif_port.c:192-220` + `io.c:1749-1774`):
///
/// | port | options | `Data` | answer | writes |
/// |---|---|---|---|---|
/// | busy | `[nosuspend]` | not iodata | atom `false` | 0 |
/// | busy | `[nosuspend]` | iodata | atom `false` | 0 |
/// | busy | none | iodata | the typed capability gap (`unsupported: … busy port …`) | 0 |
/// | busy | `[force]` on a **non**-`SOFT_BUSY` entry | iodata | `error:notsup` | 0 |
///
/// The first row is the C's ordering: the busy check runs **before** the iolist conversion
/// (`io.c:1749-1754` vs `:1790`), so a command that is dropped is never `badarg` — even when the
/// bytes could not have been converted at all. The third row is this runtime's one honest
/// divergence: the C suspends the caller and re-issues the BIF, which the `call_bif` seam cannot
/// express, so it is a typed capability gap rather than a fabricated `true`.
#[test]
fn the_busy_dropped_rows_write_nothing() {
    let mut runtime = native_runtime();
    let (port, _flag, writes) = register_busy_port(&mut runtime, "busy_cmd", true, false);
    let port_term = port.to_eterm();
    let nosuspend = atom(&mut runtime, "nosuspend");
    let force = atom(&mut runtime, "force");
    let nosuspend_only = runtime_list(&mut runtime, &[nosuspend]);
    let force_only = runtime_list(&mut runtime, &[force]);
    let not_iodata = make_small(42);
    // 1. busy + `[nosuspend]` + a `Data` that is not iodata at all ⇒ the atom `false`.
    let answer = call(
        &mut runtime,
        "erlang",
        "port_command",
        &[port_term, not_iodata, nosuspend_only],
    )
    .expect("the busy arm precedes the iodata conversion");
    assert_eq!(runtime.atom_name_of(answer), Some("false"));
    // 2. busy + `[nosuspend]` + iodata ⇒ the atom `false`.
    let data = chars(&mut runtime, "x");
    let answer = call(
        &mut runtime,
        "erlang",
        "port_command",
        &[port_term, data, nosuspend_only],
    )
    .expect("`nosuspend` on a busy port answers false");
    assert_eq!(runtime.atom_name_of(answer), Some("false"));
    // 3. busy, no options ⇒ the C suspends the caller; here, the typed capability gap.
    for args in [vec![port_term, data], vec![port_term, data, NIL]] {
        let error = call(&mut runtime, "erlang", "port_command", &args)
            .expect_err("a plain command on a busy port cannot answer");
        let reason = error::unsupported_reason(&error)
            .unwrap_or_else(|| panic!("a capability gap, not {error}"));
        assert!(
            reason.contains("busy port"),
            "the reason names the busy state: {reason}"
        );
    }
    // 4. `[force]` on an entry without `SOFT_BUSY` ⇒ `error:notsup`, busy or not.
    let error = call(
        &mut runtime,
        "erlang",
        "port_command",
        &[port_term, data, force_only],
    )
    .expect_err("a driver without SOFT_BUSY rejects `force`");
    assert_eq!(raised_text(&error), "error:notsup");
    assert_eq!(
        error::unsupported_reason(&error),
        None,
        "`notsup` is a genuine error, not a capability gap: {error}"
    );
    // Nothing above reached a write: the driver's `output` never ran and the port's own output
    // counter is still zero.
    assert_eq!(writes.get(), 0, "a dropped command writes nothing");
    assert_eq!(
        output_counter(&mut runtime, port_term),
        0,
        "`port_info(Port, output)` counts accepted bytes only"
    );
}
/// `[force]` on a **`SOFT_BUSY`** driver bypasses the busy state and the command really reaches
/// the driver: the answer is `true`, the driver's `output` callback ran **once**, and the port's
/// `output` counter is the payload's byte length.
///
/// This is the other half of [`the_busy_dropped_rows_write_nothing`]: the same busy port, the same
/// command, one entry flag apart — which is what makes the write counter evidence rather than
/// decoration. It also pins that `[force]` does **not** turn a busy port's `[nosuspend]` arm into
/// an accept: `force` widens the write, it does not change which arm answers.
#[test]
fn force_on_a_soft_busy_driver_reaches_the_driver() {
    let mut runtime = native_runtime();
    let (port, flag, writes) = register_busy_port(&mut runtime, "busy_cmd", true, true);
    let port_term = port.to_eterm();
    let force = atom(&mut runtime, "force");
    let nosuspend = atom(&mut runtime, "nosuspend");
    let force_only = runtime_list(&mut runtime, &[force]);
    let data = chars(&mut runtime, "xyz");
    let answer = call(
        &mut runtime,
        "erlang",
        "port_command",
        &[port_term, data, force_only],
    )
    .expect("a SOFT_BUSY driver accepts `force`");
    assert_eq!(runtime.atom_name_of(answer), Some("true"));
    assert_eq!(
        writes.get(),
        1,
        "the accepted command really reached the driver's `output`"
    );
    assert_eq!(
        output_counter(&mut runtime, port_term),
        3,
        "the port's output counter is the payload's byte length"
    );
    // `[nosuspend]` alone still answers `false` on the busy port — `SOFT_BUSY` only widens the
    // `force` arm — and the payload is still not written.
    let nosuspend_only = runtime_list(&mut runtime, &[nosuspend]);
    let answer = call(
        &mut runtime,
        "erlang",
        "port_command",
        &[port_term, data, nosuspend_only],
    )
    .expect("`nosuspend` answers false");
    assert_eq!(runtime.atom_name_of(answer), Some("false"));
    assert_eq!(writes.get(), 1, "the dropped command wrote nothing");
    // Idle now: the plain command is accepted, so the counter moves to two writes and six bytes.
    flag.set_busy(false);
    let answer = call(&mut runtime, "erlang", "port_command", &[port_term, data])
        .expect("an idle port accepts the write");
    assert_eq!(runtime.atom_name_of(answer), Some("true"));
    assert_eq!(writes.get(), 2);
    assert_eq!(output_counter(&mut runtime, port_term), 6);
}
