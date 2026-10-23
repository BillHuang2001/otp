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
//! The `[force]` arm of `erlang:port_command/2,3` on a **built-in** (statically linked) driver
//! port — `{spawn_driver, "tcp_inet" | "udp_inet" | "ram_file_drv"}`, the three names in
//! `beam_io::STATIC_SPAWN_DRIVERS` (`driver_tab.c`).
//!
//! # Why this is not the driver-name matrix it looks like
//!
//! The C refuses `[force]` with `error:notsup` for any driver whose **entry** lacks
//! `ERL_DRV_FLAG_SOFT_BUSY` (`erl_bif_port.c:195-198`), and the emulator's own entries split:
//! `tcp_inet` declares `ERL_DRV_FLAG_USE_PORT_LOCKING|ERL_DRV_FLAG_SOFT_BUSY`
//! (`erts/emulator/drivers/common/inet_drv.c:1374`) while `udp_inet` (`:1432`) and `ram_file_drv`
//! (`ram_file_drv.c:116`) declare no `SOFT_BUSY` — so the oracle answers `true` for the first and
//! `error:notsup` for the other two.
//!
//! This runtime's built-in drivers carry the same word as [`beam_io::Driver::flags`]
//! (`TcpInetDriver` returns `DriverFlags::SOFT_BUSY`, its siblings the empty set), and
//! `crate::port::PortMeta::driver_busy` mirrors it into `PortRegistry::soft_busy` at `open_port`
//! time — read from the driver's own advertisement while the concrete driver is still in hand,
//! never from its name. These tests pin the mirror's source (the flag word, through the registry)
//! **and** its two observables (the BIF's `true` / `error:notsup`), against the live oracle.
use super::tests::raised_text;
use super::*;
use crate::native::test_support::{atom, call, runtime as native_runtime};
use crate::runtime::Runtime;
use beam_term::port_number;
/// The emulator's three statically linked driver names, in `beam_io::STATIC_SPAWN_DRIVERS` order —
/// each with the `SOFT_BUSY` answer its C entry gives (`inet_drv.c:1374`, `:1432`,
/// `ram_file_drv.c:116`).
const DRIVERS: [(&str, bool); 3] = [
    ("ram_file_drv", false),
    ("tcp_inet", true),
    ("udp_inet", false),
];
/// A well-formed `tcp_inet` command payload: the `prim_inet` caller-tag prefix
/// (`erts/preloaded/src/prim_inet.erl:575-585`) with a **zero-length** tag, then `"abc"`.
///
/// Only `tcp_inet` ever reaches a driver `output` in these probes (the other two refuse `[force]`
/// before any conversion), and this shape is what keeps that write on the C's **unconnected**
/// arm (`inet_drv.c:12674-12683`: the command is accepted, answers `{error,enotconn}` and writes
/// nothing) instead of the malformed-command one, so the probe measures the `[force]` decision and
/// not `beam_io`'s framing.
const TCP_COMMAND: &[u8] = &[0, 0, 97, 98, 99];
/// Opens `{spawn_driver, name}` with no options and returns the port term and its id.
fn open(runtime: &mut Runtime, name: &str) -> (Eterm, PortId) {
    let driver = atom(runtime, "spawn_driver");
    let driver_name = crate::native::test_support::chars(runtime, name);
    let port_name = crate::port::tests::runtime_tuple(runtime, &[driver, driver_name]);
    let term = call(runtime, "erlang", "open_port", &[port_name, NIL])
        .unwrap_or_else(|error| panic!("open_port({{spawn_driver,{name:?}}}, []) : {error}"));
    (term, PortId::new(port_number(term)))
}
/// A `[force]` option list on the runtime's own heap.
fn force_only(runtime: &mut Runtime) -> Eterm {
    let force = atom(runtime, "force");
    crate::port::tests::runtime_list(runtime, &[force])
}
/// The `[force]` answer of `erlang:port_command(Port, Data, [force])`, rendered the way the oracle
/// probe below renders it: `true` / `false` / `error:<Reason>`.
fn forced(runtime: &mut Runtime, port: Eterm, data: &[u8]) -> String {
    let payload = terms::make_heap_binary(&mut RuntimeContext::new(&mut *runtime), data)
        .expect("a small heap binary");
    let options = force_only(runtime);
    match call(runtime, "erlang", "port_command", &[port, payload, options]) {
        Ok(answer) => runtime
            .atom_name_of(answer)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{answer:?}")),
        Err(error) => raised_text(&error),
    }
}
/// The oracle's session: for every name, the `[force]` answer of one `<<0,0,97,98,99>>` command
/// on a freshly opened `{spawn_driver, Name}` port, plus whether the port survived it —
/// `name=<answer>/<is_port>` per driver, comma-joined.
///
/// A fresh port per driver is deliberate: `tcp_inet`'s forced command really reaches the driver, so
/// a shared port would let one driver's state leak into the next reading.
fn oracle_expression() -> String {
    format!(
        "logger:set_primary_config(level, none), \
         F = fun(Name) -> \
               P = open_port({{spawn_driver, Name}}, []), \
               R = try port_command(P, {data}, [force]) \
                   of V -> lists:flatten(io_lib:format(\"~p\", [V])) \
                   catch error: E -> \"error:\" ++ lists:flatten(io_lib:format(\"~p\", [E])) \
                   end, \
               Live = is_port(P), \
               catch port_close(P), \
               Name ++ \"=\" ++ R ++ \"/\" ++ atom_to_list(Live) \
             end, \
         io:format(\"~s\", [lists:join(\",\", [F(N) || N <- [{names}]])]).",
        data = erl_binary(TCP_COMMAND),
        names = DRIVERS
            .iter()
            .map(|(name, _)| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(",")
    )
}
/// `bytes` as the Erlang binary literal both sides are written with.
fn erl_binary(bytes: &[u8]) -> String {
    let rendered = bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{rendered}>>")
}
/// The runtime's own answer for the same session, rendered identically.
fn session(runtime: &mut Runtime) -> String {
    let mut rendered = Vec::new();
    for (name, _) in DRIVERS {
        let (port, _id) = open(runtime, name);
        let answer = forced(runtime, port, TCP_COMMAND);
        let live = call(runtime, "erlang", "is_port", &[port])
            .expect("is_port/1")
            .eq(&atom(runtime, "true"));
        // Closing the port keeps the next reading independent of this one.
        let _ = call(runtime, "erlang", "port_close", &[port]);
        rendered.push(format!(
            "{name}={answer}/{}",
            if live { "true" } else { "false" }
        ));
    }
    rendered.join(",")
}
/// The `[force]` answers on the three statically linked drivers agree with the live oracle, and
/// the port survives every one of them.
///
/// This is the differential evidence for the `SOFT_BUSY` mirror: `tcp_inet` is `true` (its entry
/// declares the bit), `udp_inet`/`ram_file_drv` are `error:notsup` (theirs do not), and no arm
/// closes the port — the `[force]` refusal happens before any output is attempted.
#[test]
fn the_builtin_force_answers_agree_with_the_oracle() {
    let observed = session(&mut native_runtime());
    match super::ram_file_matrix_tests::oracle_output(&oracle_expression()) {
        Some(oracle) => assert_eq!(
            observed, oracle,
            "`port_command(P, Data, [force])` must answer like the oracle on every static driver"
        ),
        None => super::tests::skip_oracle_check("the built-in drivers' `[force]` answers"),
    }
}
/// The same matrix with no oracle on `PATH`: the answers this runtime must give on its own.
///
/// `tcp_inet` is the oracle's `true` — its `Driver::flags` carries `SOFT_BUSY`, which is the very
/// bit the C's `[force]` test reads (`inet_drv.c:1374`) — while `udp_inet` (`:1432`) and
/// `ram_file_drv` leave the word empty and stay `error:notsup`. The `notsup` must be a **genuine
/// error**, not a capability gap: the differential harness turns any `unsupported`-prefixed reason
/// into the exit-3 signal.
#[test]
fn the_builtin_force_answers_hold_without_the_oracle() {
    let mut runtime = native_runtime();
    for (name, soft_busy) in DRIVERS {
        let (port, _id) = open(&mut runtime, name);
        let payload = terms::make_heap_binary(&mut RuntimeContext::new(&mut runtime), TCP_COMMAND)
            .expect("a small heap binary");
        let options = force_only(&mut runtime);
        let answer = call(&mut runtime, "erlang", "port_command", &[port, payload, options]);
        if soft_busy {
            let answer = answer.unwrap_or_else(|error| {
                panic!("`[force]` on {name:?} must be accepted, not {error}")
            });
            assert_eq!(
                runtime.atom_name_of(answer),
                Some("true"),
                "`[force]` on {name:?} is the oracle's `true`"
            );
        } else {
            let error = answer.expect_err("a driver without SOFT_BUSY rejects `force`");
            assert_eq!(raised_text(&error), "error:notsup", "on {name:?}");
            assert_eq!(
                error::unsupported_reason(&error),
                None,
                "`notsup` is a genuine error, not a capability gap: {error}"
            );
        }
        // Neither arm closes the port: the `[force]` decision precedes every write.
        let live = call(&mut runtime, "erlang", "is_port", &[port]).expect("is_port/1");
        assert_eq!(
            runtime.atom_name_of(live),
            Some("true"),
            "{name:?} survives its `[force]` answer"
        );
        let _ = call(&mut runtime, "erlang", "port_close", &[port]);
    }
}
/// The registry's `soft_busy` mirror is the built-in driver's **own** flag word, and its busy half
/// stays `false` for a built-in port (that kind of driver reports `is_busy` itself).
///
/// This is the source-of-truth half of the suite: it reads [`PortRegistry::soft_busy`] directly, so
/// a future hardcoded driver-name list would show up here as a mirror that agrees with the BIF but
/// not with `beam_io`, and a mirror read from the wrong place would show up as a wrong cell.
#[test]
fn the_builtin_soft_busy_mirror_is_the_drivers_own_flag_word() {
    let mut runtime = native_runtime();
    for (name, soft_busy) in DRIVERS {
        let (_term, id) = open(&mut runtime, name);
        let ports = std::rc::Rc::clone(&runtime.ports);
        let registry = ports.try_borrow().expect("the port table");
        assert!(registry.contains(id), "{name:?} is registered");
        assert_eq!(
            registry.soft_busy(id),
            soft_busy,
            "{name:?}'s `ERL_DRV_FLAG_SOFT_BUSY` mirror"
        );
        assert!(
            !registry.is_busy(id),
            "{name:?} is not busy: a built-in driver reports its state through `Driver::is_busy`"
        );
    }
}
