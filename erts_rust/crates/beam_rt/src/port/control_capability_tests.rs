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
//! The **driver-`control` capability** matrix behind `erlang:port_control/3` (contract A.4).
//!
//! `erlang:port_control/3` reaches a driver's `control` callback only if the port's driver
//! **declares** one: the C answers `ERTS_PORT_OP_BADARG` before any callback for a driver whose
//! `prt->drv_ptr->control` slot is `NULL` (`../../erts/emulator/beam/io.c:4035`), and
//! `erts_internal_port_control_3` turns that into the atom `badarg`
//! (`../../erts/emulator/beam/erl_bif_port.c:340-343`). So the capability — not the verb, and not
//! the port's `Name` form — decides whether a call is answered at all:
//!
//! * `{spawn_driver, "ram_file_drv"}` — the emulator's one static driver whose entry has
//!   `NULL, /* control */` (`../../erts/emulator/drivers/common/ram_file_drv.c:116`) — is
//!   `error:badarg` for **every** verb, including the verbs it happily answers through
//!   `port_command/2` (`ram_file.erl`'s only transport, measured on erts-17.1);
//! * `{spawn_driver, "udp_inet"}`, the *same* `Name` form, declares `inet_control`
//!   (`erts/emulator/drivers/common/inet_drv.c:1348`) and keeps answering its numeric verbs — and
//!   answers the driver's own "no such opcode" reply for the bare numbers the `inet` magic prefix
//!   does not cover;
//! * an **internal** port (`{spawn, Cmd}` / `{fd, N}`) is backed by a driver with no `control`
//!   callback at all, so it is `badarg` for every verb too.
//!
//! The gate is [`PortRegistry::has_driver_control`](super::PortRegistry::has_driver_control); the
//! capability itself is the driver's own — [`beam_io::Driver::has_control`], `false` only for a
//! driver whose C counterpart leaves `control` `NULL` (the built-in `ram_file_drv`; a `dlopen`ed
//! entry through its own `ErlDrvEntry::control` slot) — captured into the port when it is opened.//!
//! # What is compared, and how
//!
//! [`the_capability_matrix_matches_the_oracle`] drives the same labelled probe set through this
//! runtime's [`RuntimePorts::port_control`] and through the system `erl`'s
//! `erlang:port_control/3`, and compares the two **whole-session renderings** line for line — the
//! oracle's probe normalises a raise to the bare atom (`catch error:badarg -> badarg`) so both
//! sides print one line per probe. A missing toolchain is a **loud skip**, never a silent pass
//! (the convention of `../port/tests.rs` and `ram_file_matrix_tests.rs`).
//!
//! The per-arm tests then pin each verdict in typed form — `Err(BeamError::BadArg)` for the
//! driverless arms, the exact reply bytes for the control-bearing one — so a regression names the
//! arm that broke rather than only the session line.
//!
//! Two `badarg`s live here and they must not be confused: the **capability** refusal (verb-blind,
//! raised before the driver is reached) and the **driver's own** rejection of an operand (per-verb,
//! the C's negative `cres`, `io.c:4074`). They converge on the same exception atom; the
//! distinction is the *cause*, and the udp_inet arm below — where the same bare verb `1` that
//! `ram_file_drv` refuses outright is answered with the driver's own reply — is what tells them
//! apart.
use super::tests::{data_payload_bytes, erl_bytes, erl_eval, fixture, skip_oracle_check, Fixture};
use super::*;
use crate::config::RuntimeConfig;

/// The `inet` drivers' opcode-magic prefix: `erlang:port_control/3`'s integer `Op` carries
/// `magic + switch` for the socket drivers (`beam_io`'s `drivers/inet/opcode.rs`, the C's
/// `inet_ctl` decoding at `erts/emulator/drivers/common/inet_drv.c`).
const INET_OPCODE_MAGIC: u32 = 0x03f1_a300;

/// The socket drivers' answer to a number the magic prefix does not cover — the C's
/// `[0] "exbadport"` reply.
const INET_EXBADPORT: &[u8] = &[0, 101, 120, 98, 97, 100, 112, 111, 114, 116];

/// One probe of the matrix: the label both renderings print, the `Operation` integer and the
/// `Data` operand bytes (which `port_control/3` reads as iodata, so they are the payload verbatim).
struct Probe {
    /// The line label, spelled identically on both sides so the sessions are comparable.
    label: &'static str,
    /// The `Operation` integer.
    verb: u32,
    /// The `Data` operand.
    data: &'static [u8],
}

/// The `{spawn_driver, "ram_file_drv"}` probes — **all** of them refused, including `1`
/// (`RAM_FILE_OPEN`, the verb the driver answers through `port_command/2`) and `38` (`ADVISE`,
/// which the driver's Rust `control` would answer `OK` for).
const RAM_FILE_PROBES: &[Probe] = &[
    Probe {
        label: "ram_open",
        verb: 1,
        data: &[0, 0, 0, 3, 97, 98, 99],
    },
    Probe {
        label: "ram_advise",
        verb: 38,
        data: &[],
    },
    Probe {
        label: "ram_unknown",
        verb: 99,
        data: &[],
    },
];

/// The `{spawn_driver, "udp_inet"}` probes — answered, because that driver declares `control`:
/// `Open`, `Setopts` and `Gettype` under the magic prefix, then two bare numbers (the driver's own
/// `exbadport` refusal, which is **not** the capability `badarg`).
const UDP_PROBES: &[Probe] = &[
    Probe {
        label: "udp_open",
        verb: INET_OPCODE_MAGIC + 1,
        data: &[1, 2, 0],
    },
    Probe {
        label: "udp_setopts",
        verb: INET_OPCODE_MAGIC + 7,
        data: &[],
    },
    Probe {
        label: "udp_gettype",
        verb: INET_OPCODE_MAGIC + 15,
        data: &[],
    },
    Probe {
        label: "udp_bare_1",
        verb: 1,
        data: &[],
    },
    Probe {
        label: "udp_bare_99",
        verb: 99,
        data: &[],
    },
];

/// The internal-port probes: a `{spawn, Cmd}` port's driver has no `control` callback, so both
/// verbs are the capability `badarg`.
const INTERNAL_PROBES: &[Probe] = &[
    Probe {
        label: "cat_one",
        verb: 1,
        data: &[],
    },
    Probe {
        label: "cat_unknown",
        verb: 99,
        data: &[],
    },
];

/// A live runtime with the fixture's atom table and a [`RuntimePorts`] built over the runtime's
/// **own** port registry, so a port opened here is the port the runtime's poller and its
/// `port_command` path see.
struct Session {
    /// The runtime whose registry every port is opened on.
    runtime: Runtime,
    /// The term/heap fixture (the `Name`/`Options` vocabulary and the term arena).
    fx: Fixture,
    /// The provider under test.
    hook: RuntimePorts,
}

impl Session {
    /// A fresh runtime with no port open.
    fn new() -> Session {
        let fx = fixture();
        let runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let owner = runtime.self_pid();
        let hook = fx.hook_over(Rc::clone(&runtime.ports), owner);
        Session { runtime, fx, hook }
    }

    /// Opens `{spawn_driver, name}` (or `{spawn, cmd}` when `name` is a shell command — see
    /// [`Session::open_spawn`]) with `options`.
    fn open_driver(&mut self, name: &str, options: Eterm) -> PortId {
        let name = self.fx.string(name);
        let name = self.fx.name("spawn_driver", name);
        self.hook
            .open_port(name, options)
            .unwrap_or_else(|error| panic!("open_port({{spawn_driver,{name:?}}}) failed: {error}"))
    }

    /// Opens `{spawn, cmd}` with the default (`list`) stream.
    fn open_spawn(&mut self, cmd: &str) -> PortId {
        let name = self.fx.string(cmd);
        let name = self.fx.name("spawn", name);
        self.hook
            .open_port(name, NIL)
            .unwrap_or_else(|error| panic!("open_port({{spawn,{cmd:?}}}) failed: {error}"))
    }

    /// The `[binary]` `open_port/2` option list.
    fn binary_option(&mut self) -> Eterm {
        let binary = self.fx.atom("binary");
        self.fx.list(&[binary])
    }

    /// Whether the runtime considers `port`'s driver to declare a `control` callback.
    fn has_driver_control(&self, port: PortId) -> bool {
        self.runtime.ports.borrow().has_driver_control(port)
    }

    /// Drives one probe and renders its outcome the way the oracle's own probe prints it: the
    /// reply as an Erlang term (a binary when the driver asked for
    /// `PORT_CONTROL_FLAG_BINARY`, an int list otherwise), or the bare atom `badarg`.
    fn control_line(&mut self, port: PortId, probe: &Probe) -> String {
        let reply = self.hook.port_control(port, &probe.verb.to_string(), probe.data);
        // The flag is read **after** the call: that is when the driver's callback has had its
        // chance to set it (`write_port_control_result`, `io.c:4133-4175`).
        let binary = self.runtime.ports.borrow().control_binary(port);
        let rendered = match reply {
            Ok(bytes) => match binary {
                true => erl_binary(&bytes),
                false => erl_bytes(&bytes.iter().map(|byte| i64::from(*byte)).collect::<Vec<_>>()),
            },
            Err(BeamError::BadArg) => "badarg".to_string(),
            // A capability gap (or any other typed failure) is no verdict of this matrix: render
            // it loudly so the session comparison fails with the real cause in the message.
            Err(other) => format!("error:{other}"),
        };
        format!("{} {}", probe.label, rendered)
    }
}

/// `bytes` rendered the way `~w` prints a binary: `<<1,2,0>>`.
fn erl_binary(bytes: &[u8]) -> String {
    let rendered: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("<<{}>>", rendered.join(","))
}

/// The labelled `port_control/3` verdicts of the whole matrix, in probe order.
///
/// The three ports are opened in the same order on both sides, so the session is comparable
/// line for line.
fn rust_session() -> Vec<String> {
    let mut session = Session::new();
    let mut lines = Vec::new();

    let ram = session.open_driver("ram_file_drv", NIL);
    assert!(
        !session.has_driver_control(ram),
        "ram_file_drv's C entry declares no control callback"
    );
    for probe in RAM_FILE_PROBES {
        lines.push(session.control_line(ram, probe));
    }
    session.hook.close_port(ram).expect("closing the ram port");

    let binary = session.binary_option();
    let udp = session.open_driver("udp_inet", binary);
    assert!(
        session.has_driver_control(udp),
        "udp_inet's C entry declares a control callback"
    );
    for probe in UDP_PROBES {
        lines.push(session.control_line(udp, probe));
    }
    session.hook.close_port(udp).expect("closing the udp port");

    let cat = session.open_spawn("cat");
    assert!(
        !session.has_driver_control(cat),
        "an internal port's driver has no control callback"
    );
    for probe in INTERNAL_PROBES {
        lines.push(session.control_line(cat, probe));
    }
    session.hook.close_port(cat).expect("closing the spawn port");
    lines
}

/// The oracle's expression for the very same session, generated from the same probe tables so the
/// two can never drift apart.
///
/// The `try … catch error:badarg -> badarg` wrapper is what makes the oracle print the bare atom
/// instead of an `{'EXIT', {badarg, Stacktrace}}` tuple, i.e. the same text this side's
/// [`Session::control_line`] renders.
fn oracle_expression() -> String {
    let mut expr = String::from(
        "F = fun(P,V,D) -> try port_control(P,V,D) of R -> R catch error:badarg -> badarg end end,",
    );
    let ports = [
        ("P1", "open_port({spawn_driver,\"ram_file_drv\"},[])", RAM_FILE_PROBES),
        (
            "P2",
            "open_port({spawn_driver,\"udp_inet\"},[binary])",
            UDP_PROBES,
        ),
        ("P3", "open_port({spawn,\"cat\"},[])", INTERNAL_PROBES),
    ];
    for (var, open, probes) in ports {
        expr.push_str(&format!("{var} = {open},"));
        for probe in probes {
            expr.push_str(&format!(
                "io:format(\"{} ~w~n\",[F({var},{},{})]),",
                probe.label,
                probe.verb,
                erl_binary(probe.data)
            ));
        }
        expr.push_str(&format!("port_close({var}),"));
    }
    expr.push_str("ok.");
    expr
}

/// The whole matrix agrees with the live oracle — the pin for contract A.4's
/// `port_control` capability gate.
#[test]
fn the_capability_matrix_matches_the_oracle() {
    let ours = rust_session();
    match erl_eval(&oracle_expression()) {
        Some(oracle) => assert_eq!(
            ours.join("\n"),
            oracle,
            "the port_control capability matrix must match the oracle line for line"
        ),
        None => skip_oracle_check(
            "the `erlang:port_control/3` driver-capability matrix (`ram_file_drv`, `udp_inet`, a \
             `{spawn, Cmd}` port)",
        ),
    }
    // The matrix also holds on its own, with no oracle: the ram_file_drv lines are all `badarg`.
    for line in &ours[..RAM_FILE_PROBES.len()] {
        assert!(
            line.ends_with(" badarg"),
            "every `ram_file_drv` verb is the capability badarg, got {line:?}"
        );
    }
}

/// A built-in driver with **no** `control` callback is `badarg` for every verb — and is still a
/// live, answering driver, which is what makes this a capability verdict rather than a dead port.
#[test]
fn a_driver_without_a_control_callback_is_badarg_for_every_verb() {
    let mut session = Session::new();
    let port = session.open_driver("ram_file_drv", NIL);
    assert!(
        !session.has_driver_control(port),
        "ram_file_drv declares no control callback"
    );

    for probe in RAM_FILE_PROBES {
        assert!(
            matches!(
                session
                    .hook
                    .port_control(port, &probe.verb.to_string(), probe.data),
                Err(BeamError::BadArg)
            ),
            "port_control(ram_file_drv, {}, {:?}) must be badarg",
            probe.verb,
            probe.data
        );
    }
    // A verb the named-verb transport of the very same driver would answer is refused too: the
    // gate never looks at the verb.
    assert!(
        matches!(session.hook.port_control(port, "size", &[]), Err(BeamError::BadArg)),
        "even the driver's own named verb is unreachable through port_control/3"
    );

    // The port is untouched by the refusals: it is still live, and the driver still answers its
    // real transport (`port_command/2` — `RAM_FILE_SIZE`, opcode 37, on an empty file).
    assert!(session.runtime.ports.borrow().is_live(port), "still a live port");
    session
        .runtime
        .ports
        .borrow_mut()
        .write_command(port, &[37])
        .expect("the RAM_FILE_SIZE command is written");
    let messages = RuntimeContext::new(&mut session.runtime)
        .poll_port_messages(Duration::from_millis(50))
        .expect("the port poll must not fail");
    assert_eq!(messages.len(), 1, "the driver's one reply: {messages:?}");
    assert_eq!(
        data_payload_bytes(messages[0]),
        vec![3, 0, 0, 0, 0],
        "`NUMBER 0` — the reply the driver really produced"
    );
}

/// A built-in driver that **does** declare `control` keeps answering its verbs, byte for byte.
///
/// The two bare numbers at the end are the driver's **own** refusal (the C's `[0]"exbadport"`),
/// not the capability `badarg`: the same call on `ram_file_drv` is refused before the driver is
/// reached, which is the distinction this arm exists to make visible.
#[test]
fn a_driver_with_a_control_callback_still_answers_its_verbs() {
    let mut session = Session::new();
    let binary = session.binary_option();
    let port = session.open_driver("udp_inet", binary);
    assert!(
        session.has_driver_control(port),
        "udp_inet declares a control callback"
    );

    for (probe, expected) in UDP_PROBES.iter().zip([
        // `Open` — the driver accepts the (socket-less) stream open.
        vec![1],
        // `Setopts` with no options.
        vec![1],
        // `Gettype`: `ok` followed by the two 32-bit values (type stream, protocol 2).
        vec![1, 0, 0, 0, 1, 0, 0, 0, 2],
        // A bare number the `inet` magic prefix does not cover: the driver's own `exbadport`.
        INET_EXBADPORT.to_vec(),
        INET_EXBADPORT.to_vec(),
    ]) {
        let reply = session
            .hook
            .port_control(port, &probe.verb.to_string(), probe.data);
        // [`BeamError`] is not `PartialEq` (it carries an owned message and a live reason term),
        // so the comparison is on the payload and the shape, not on an `assert_eq!` of the result.
        match reply {
            Ok(bytes) => assert_eq!(
                bytes, expected,
                "port_control(udp_inet, {}, {:?})",
                probe.verb, probe.data
            ),
            Err(error) => panic!(
                "port_control(udp_inet, {}, {:?}) must answer, got {error}",
                probe.verb, probe.data
            ),
        }
    }
}
/// An **internal** port is `badarg` for every verb: its driver (`beam_io`'s spawn driver) has no
/// `control` callback, so the capability gate refuses it exactly as it refuses a control-less
/// built-in — the pre-existing internal-port arm, unweakened.
#[test]
fn an_internal_port_with_no_control_callback_is_badarg() {
    let mut session = Session::new();
    let port = session.open_spawn("cat");
    assert!(
        !session.has_driver_control(port),
        "an internal port's driver declares no control callback"
    );
    for probe in INTERNAL_PROBES {
        assert!(
            matches!(
                session
                    .hook
                    .port_control(port, &probe.verb.to_string(), probe.data),
                Err(BeamError::BadArg)
            ),
            "port_control({{spawn,\"cat\"}}, {}, {:?}) must be badarg",
            probe.verb,
            probe.data
        );
    }
}
