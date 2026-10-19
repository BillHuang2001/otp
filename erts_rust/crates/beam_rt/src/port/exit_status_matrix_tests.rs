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
//! The `[exit_status]` **name-form matrix**: which `open_port/2` `Name` forms accept the option and
//! which are the oracle's `badarg`.
//!
//! # The rule being pinned
//!
//! The C refuses the option for every port whose `Name` resolves to a **registered driver** — never
//! for the fallback `spawn_driver`:
//!
//! ```c
//! if (driver == NULL || (driver != &spawn_driver && opts->exit_status)) {
//!     erts_rwmtx_runlock(&erts_driver_list_lock);
//!     ERTS_OPEN_DRIVER_RET(NULL, -3, BADARG);
//! }
//! ```
//!
//! (`erts/emulator/beam/io.c:623-626`). Two facts follow from that one line, and both are what this
//! matrix exists for:
//!
//! * the **name form alone** does not decide it — `{spawn, "tcp_inet"}` walks the command's first
//!   word through the loaded-driver list (`io.c:603-621`) and finds `tcp_inet`, so it is refused
//!   exactly like `{spawn_driver, "tcp_inet"}`; the same command spelled
//!   `{spawn, "no_such_drv_x"}` finds no driver, falls back to `&spawn_driver`, and is accepted;
//! * a `{fd, N}` port is backed by `fd_driver` — another real driver — so it is refused too, even
//!   though the emulator's own file-descriptor driver reaps no child at all.
//!
//! The runtime's predicate is [`super::opens_a_driver_port`], and it runs **before any registry
//! call** (`super::RuntimePorts::open_port`'s `resolved.exit_status && opens_a_driver_port(&spec)`
//! arm), so a refusal can never consume a port number — the second test below pins that.
//!
//! # Why the refusal is the whole truth, not a shortcut
//!
//! The only producer of a `{Port, {exit_status, Status}}` message for a **driver** port is the
//! emulator-internal `driver_report_exit` (`erts/emulator/beam/io.c:5554`), which `erl_driver.h`
//! does not declare: no Erlang-reachable driver can report one, so the option can never mean
//! anything on such a port. Accepting it would promise a message that cannot exist.
//!
//! # How the two sides are compared
//!
//! One table drives both runtimes: each row is a label, a `Name` form and an option list. Each side
//! renders the row as `label=ok` or `label=error:<reason>` — the oracle through
//! `io_lib:format("~p", …)`, this runtime from the `BeamError` — and the **whole session** (all
//! rows, in table order, comma-joined) must be identical. A missing `erl` is a loud skip
//! ([`skip_oracle_check`]), never a silent pass; the runtime-side assertions of
//! [`the_refused_rows_are_badarg_without_the_oracle`] hold whether or not the oracle ran.
//!
//! A `{spawn, Cmd}` row really forks `/bin/sh -c Cmd`, so a row whose command names no program
//! writes `sh: line 1: exec: …: not found` to **stderr** on both sides; the oracle probe captures
//! with `Command::output()` and compares stdout only, so that noise never enters the comparison.
//!
//! # Oracle-reachable scope only
//!
//! Every row here is a real `open_port/2` both runtimes can execute. The `{fd, N}` row is the one
//! exception in *this* runtime: it is opened with `[exit_status]`, so the refusal happens before the
//! descriptor is ever adopted — a bare `{fd, 1}` is **never** opened here, because that would adopt
//! the test process's own stdout.
use super::ram_file_matrix_tests::oracle_output;
use super::tests::{assert_badarg, raised_text, runtime_list, runtime_tuple, skip_oracle_check};
use super::*;
use crate::native::test_support::{atom, call, chars, runtime as native_runtime};
/// One `open_port/2` `Name` form the matrix drives.
#[derive(Clone, Copy)]
enum NameForm {
    /// `{spawn_driver, Name}` — a name the driver list must already hold.
    Driver(&'static str),
    /// `{spawn, CommandLine}` — resolved against the driver list first, then the shell.
    Spawn(&'static str),
    /// `{spawn_executable, Path}` with its `{args, […]}` supplied as an option.
    Executable(&'static str),
    /// `{fd, Descriptor}`.
    Fd(i64),
}
/// One element of a row's option list.
#[derive(Clone, Copy)]
enum Opt {
    /// The atom `exit_status` — the option under test.
    ExitStatus,
    /// The atom `stderr_to_stdout` — a driver-port option that is accepted and inert.
    StderrToStdout,
    /// `{args, [String]}`.
    Args(&'static [&'static str]),
}
/// One matrix row: the label both sides print, the `Name` form and the option list.
struct Row {
    /// The row label, printed identically by both sides.
    label: &'static str,
    /// The `open_port/2` `Name` operand.
    name: NameForm,
    /// The `open_port/2` `Options` operand.
    options: &'static [Opt],
}
/// The matrix, in table order: the six refused rows first, then every accepted one.
///
/// The order is load-bearing for [`a_refused_row_consumes_no_port_number`], which reuses the
/// leading refusal block verbatim so it cannot drift from the matrix.
const ROWS: &[Row] = &[
    Row {
        label: "ram_file_drv_driver_exit_status",
        name: NameForm::Driver("ram_file_drv"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "tcp_inet_driver_exit_status",
        name: NameForm::Driver("tcp_inet"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "tcp_inet_spawn_exit_status",
        name: NameForm::Spawn("tcp_inet"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "ram_file_drv_spawn_exit_status",
        name: NameForm::Spawn("ram_file_drv"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "udp_inet_spawn_exit_status",
        name: NameForm::Spawn("udp_inet"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "fd_exit_status",
        name: NameForm::Fd(1),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "ram_file_drv_driver_bare",
        name: NameForm::Driver("ram_file_drv"),
        options: &[],
    },
    Row {
        label: "tcp_inet_spawn_bare",
        name: NameForm::Spawn("tcp_inet"),
        options: &[],
    },
    Row {
        label: "fd_spawn_bare",
        name: NameForm::Spawn("fd"),
        options: &[],
    },
    Row {
        label: "fd_spawn_exit_status",
        name: NameForm::Spawn("fd"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "efile_spawn_bare",
        name: NameForm::Spawn("efile"),
        options: &[],
    },
    Row {
        label: "efile_spawn_exit_status",
        name: NameForm::Spawn("efile"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "spawn_spawn_bare",
        name: NameForm::Spawn("spawn"),
        options: &[],
    },
    Row {
        label: "spawn_spawn_exit_status",
        name: NameForm::Spawn("spawn"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "unknown_command_exit_status",
        name: NameForm::Spawn("no_such_drv_x"),
        options: &[Opt::ExitStatus],
    },
    Row {
        label: "spawn_executable_exit_status",
        name: NameForm::Executable("/bin/sh"),
        options: &[Opt::Args(&["-c", "true"]), Opt::ExitStatus],
    },
    Row {
        label: "ram_file_drv_driver_stderr_to_stdout",
        name: NameForm::Driver("ram_file_drv"),
        options: &[Opt::StderrToStdout],
    },
];
/// The number of leading [`ROWS`] entries that are the oracle's `badarg`.
///
/// The six driver-backed forms (`{spawn_driver, _}` twice, `{spawn, _}` on three driver names and
/// `{fd, 1}`) — measured on erts-17.1.
const REFUSED_ROWS: usize = 6;
/// The `Name` operand as an Erlang source expression — the exact text the oracle is driven with.
fn oracle_name(name: &NameForm) -> String {
    match name {
        NameForm::Driver(name) => format!("{{spawn_driver,{name:?}}}"),
        NameForm::Spawn(command) => format!("{{spawn,{command:?}}}"),
        NameForm::Executable(path) => format!("{{spawn_executable,{path:?}}}"),
        NameForm::Fd(descriptor) => format!("{{fd,{descriptor}}}"),
    }
}
/// One option as an Erlang source expression.
fn oracle_option(option: &Opt) -> String {
    match option {
        Opt::ExitStatus => "exit_status".to_string(),
        Opt::StderrToStdout => "stderr_to_stdout".to_string(),
        Opt::Args(args) => {
            let args = args
                .iter()
                .map(|argument| format!("{argument:?}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{args,[{args}]}}")
        }
    }
}
/// The option list as an Erlang source expression.
fn oracle_options(options: &[Opt]) -> String {
    let rendered = options
        .iter()
        .map(oracle_option)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{rendered}]")
}
/// The oracle's whole session: open every row in table order and print one `label=ok` /
/// `label=error:<Reason>` entry per row, comma-joined.
///
/// A row's port is deliberately **left open**: the compared fact is the `open_port/2` outcome
/// alone, and the VM closes everything when it stops. (Wrapping the close in the same `try` would
/// let a refused close decide the row.)
fn oracle_expression() -> String {
    let rows = ROWS
        .iter()
        .map(|row| {
            format!(
                "{{{:?},{},{}}}",
                row.label,
                oracle_name(&row.name),
                oracle_options(row.options)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "F = fun({{L, N, O}}) -> \
           R = try open_port(N, O) of _P -> \"ok\" \
               catch error: E -> \"error:\" ++ lists:flatten(io_lib:format(\"~p\", [E])) \
               end, \
           L ++ \"=\" ++ R \
         end, \
         io:format(\"~s\", [lists:join(\",\", [F(R) || R <- [{rows}]])])."
    )
}
/// The `Name` operand as the runtime's own term.
fn runtime_name(runtime: &mut Runtime, name: &NameForm) -> Eterm {
    match name {
        NameForm::Driver(name) => {
            let tag = atom(runtime, "spawn_driver");
            let value = chars(runtime, name);
            runtime_tuple(runtime, &[tag, value])
        }
        NameForm::Spawn(command) => {
            let tag = atom(runtime, "spawn");
            let value = chars(runtime, command);
            runtime_tuple(runtime, &[tag, value])
        }
        NameForm::Executable(path) => {
            let tag = atom(runtime, "spawn_executable");
            let value = chars(runtime, path);
            runtime_tuple(runtime, &[tag, value])
        }
        NameForm::Fd(descriptor) => {
            let tag = atom(runtime, "fd");
            runtime_tuple(runtime, &[tag, make_small(*descriptor)])
        }
    }
}
/// The option list as the runtime's own term.
fn runtime_options(runtime: &mut Runtime, options: &[Opt]) -> Eterm {
    let mut elements = Vec::new();
    for option in options {
        let element = match option {
            Opt::ExitStatus => atom(runtime, "exit_status"),
            Opt::StderrToStdout => atom(runtime, "stderr_to_stdout"),
            Opt::Args(args) => {
                let tag = atom(runtime, "args");
                let rendered = args
                    .iter()
                    .map(|argument| chars(runtime, argument))
                    .collect::<Vec<_>>();
                let list = runtime_list(runtime, &rendered);
                runtime_tuple(runtime, &[tag, list])
            }
        };
        elements.push(element);
    }
    runtime_list(runtime, &elements)
}
/// One row on this runtime, rendered exactly as [`oracle_expression`] renders it:
/// `label=ok` or `label=error:<reason>`.
///
/// The port is closed again so the session leaves no live port behind (a `{spawn, Cmd}` row really
/// forked a child). The close's own outcome is **not** part of the compared value: a row whose
/// shell could not exec anything exits immediately, and the port is then already gone by the time
/// the cleanup runs — that is the child's race, not the option's decision.
fn runtime_row(runtime: &mut Runtime, row: &Row) -> String {
    let name = runtime_name(runtime, &row.name);
    let options = runtime_options(runtime, row.options);
    match call(runtime, "erlang", "open_port", &[name, options]) {
        Ok(port) => {
            let _ = call(runtime, "erlang", "port_close", &[port]);
            format!("{}=ok", row.label)
        }
        Err(error) => format!("{}={}", row.label, raised_text(&error)),
    }
}
/// The runtime's answer for the whole matrix, comma-joined — the exact string
/// [`oracle_expression`] prints.
fn runtime_session() -> String {
    let mut runtime = native_runtime();
    ROWS.iter()
        .map(|row| runtime_row(&mut runtime, row))
        .collect::<Vec<_>>()
        .join(",")
}
/// The whole matrix agrees with the live oracle, row for row and in table order.
///
/// This is the suite's primary evidence: the same seventeen `open_port/2` calls run under both
/// runtimes, and every `ok`/`error:badarg` decision must match character for character.
#[test]
fn the_exit_status_matrix_agrees_with_the_oracle() {
    let observed = runtime_session();
    match oracle_output(&oracle_expression()) {
        Some(oracle) => assert_eq!(
            observed, oracle,
            "`[exit_status]` must be refused for exactly the driver-backed `Name` forms the \
             oracle refuses it for"
        ),
        None => skip_oracle_check("the `[exit_status]` name-form matrix"),
    }
}
/// The six driver-backed rows are `error:badarg` on this runtime **alone** — the assertion that
/// holds even with no oracle on `PATH`.
///
/// `{spawn, "tcp_inet"}`/`{spawn, "ram_file_drv"}`/`{spawn, "udp_inet"}` are the rows a
/// form-based rule would get wrong: they are `{spawn, _}`, and they are still refused because the
/// command's first word names a **loaded driver**.
#[test]
fn the_refused_rows_are_badarg_without_the_oracle() {
    let mut runtime = native_runtime();
    for row in &ROWS[..REFUSED_ROWS] {
        let name = runtime_name(&mut runtime, &row.name);
        let options = runtime_options(&mut runtime, row.options);
        let result = call(&mut runtime, "erlang", "open_port", &[name, options]);
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "{}: `[exit_status]` is badarg for a driver port, got {result:?}",
            row.label
        );
    }
    // ...and the accepted rows really do open (so the refusals above are not a blanket refusal of
    // every name form).
    let mut runtime = native_runtime();
    for row in &ROWS[REFUSED_ROWS..] {
        let name = runtime_name(&mut runtime, &row.name);
        let options = runtime_options(&mut runtime, row.options);
        let port = call(&mut runtime, "erlang", "open_port", &[name, options])
            .unwrap_or_else(|error| panic!("{}: expected a port, got {error}", row.label));
        let _ = call(&mut runtime, "erlang", "port_close", &[port]);
    }
}
/// A refused `[exit_status]` consumes **no port number**: the check runs before any registry call,
/// so the first port this runtime ever opens still gets the registry's first id.
///
/// This is the C's own ordering — `io.c:623` runs before the driver's `start`/`open` and before
/// `erts_ioq` allocates anything — and it is what keeps a refusal from leaving a hole in the port
/// table. The refusal block is [`ROWS`]'s leading [`REFUSED_ROWS`] entries verbatim, so the matrix
/// and this test cannot drift apart.
#[test]
fn a_refused_row_consumes_no_port_number() {
    let mut runtime = native_runtime();
    for row in &ROWS[..REFUSED_ROWS] {
        let name = runtime_name(&mut runtime, &row.name);
        let options = runtime_options(&mut runtime, row.options);
        assert_badarg(call(&mut runtime, "erlang", "open_port", &[name, options]));
    }
    let name = runtime_name(&mut runtime, &NameForm::Driver("ram_file_drv"));
    let port = call(&mut runtime, "erlang", "open_port", &[name, NIL]).expect("a static name opens");
    let port = PortId::from_eterm(port).expect("a port term");
    assert_eq!(port, PortId::new(0), "no refusal consumed a port number");
    let _ = call(&mut runtime, "erlang", "port_close", &[port.to_eterm()]);
}
