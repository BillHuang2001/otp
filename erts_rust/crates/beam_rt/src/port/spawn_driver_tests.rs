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
//! The `{spawn_driver, Name}` **namespace** suite: which names open a port, which are the oracle's
//! `badarg`, and the `erl_ddll:loaded_drivers/0` list that has to agree with both.
//!
//! The namespace is the emulator's **statically linked** driver set — the entries the generated
//! `driver_tab.c` table holds, i.e. `tcp_inet`, `udp_inet` and `ram_file_drv` — and nothing else.
//! `beam_io` is the one source of truth for that set (`STATIC_SPAWN_DRIVERS` /
//! `is_static_spawn_driver` / `DriverRegistry::create_static_spawn`), because the same list is what
//! `open_port({spawn_driver, Name})` resolves and what `erl_ddll:loaded_drivers/0` reports; this
//! crate holds no copy of it. The gate is what makes this crate's **internal** drivers — `fd`, a
//! byte stream over a pipe/fd, and `efile`, its file sibling — unreachable through the namespace
//! even though `beam_io` can build both: the oracle refuses `{spawn_driver,"fd"}` and
//! `{spawn_driver,"efile"}` with `badarg` because they are not emulator drivers at all. Their own
//! `Name` forms (`{fd, N}`, `{spawn, Cmd}`, `{spawn_executable, Path}`) are untouched by the gate —
//! they never reach it.
//!
//! # The measured oracle (OTP 29.1.1 / erts-17.1)
//!
//! `open_port({spawn_driver, Name}, [])` for `tcp_inet`/`udp_inet`/`ram_file_drv` answers a live
//! port whose `erlang:port_info(P, name)` is `{name, Name}`; for `fd`, `efile` and any invented name
//! it raises `error:badarg`; and `erl_ddll:loaded_drivers/0` answers
//! `{ok,["tcp_inet","udp_inet","ram_file_drv"]}` — the statics only, in `driver_tab.c` order.
//!
//! # What is pinned here
//!
//! * the runtime's answer for **all six** names (a port reporting its own name, or `badarg`) is
//!   compared **live** with the oracle's, name for name;
//! * the `erl_ddll:loaded_drivers/0` **list**, through the runtime's own BIF dispatch, is compared
//!   with the oracle's — order included, because that surface is order-sensitive;
//! * a refused name consumed no port number (validation precedes allocation), so a refusal can never
//!   leave a hole in the port table;
//! * the arrangement [`driver::static_drivers`] adds on top of `beam_io`'s set is a **permutation**
//!   of it — it can neither add nor hide a driver, which is what keeps `open_port` and
//!   `loaded_drivers/0` one view.
//!
//! A missing `erl` is a **loud skip** (the crate's harness convention, see
//! [`skip_oracle_check`]), never a silent pass. The runtime-side assertions in every test hold with
//! or without the oracle.
use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_term::{signed_value, NIL};

use super::tests::{assert_badarg, erl_eval, fixture, skip_oracle_check, text_of, Fixture};
use super::*;
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

/// The names the oracle refuses with `badarg`: this crate's two **internal** drivers plus a name
/// nothing registers. Measured on erts-17.1 — `{spawn_driver,"fd"}` and `{spawn_driver,"efile"}`
/// are `{error,badarg}`, not merely a missing `.so`.
const REFUSED: [&str; 3] = ["fd", "efile", "nosuch_driver"];

/// Every name this suite probes: the emulator's statics (in the order the `erl_ddll` surface reports
/// them) followed by the refused ones.
fn probe_names() -> Vec<&'static str> {
    let mut names = driver::static_drivers();
    names.extend(REFUSED);
    names
}

/// The runtime's answer for `name` in the shape both sides print:
/// `name=port:<reported name>` or `name=error:<reason>`.
///
/// The port is closed again so the fixture stays usable across the whole name list (each `tcp_inet`
/// port really holds a socket).
fn runtime_probe(fx: &mut Fixture, name: &str) -> String {
    let command = fx.string(name);
    let spec = fx.name("spawn_driver", command);
    match fx.hook.open_port(spec, NIL) {
        Ok(port) => {
            let reported = text_of(fx.item(port, "name"));
            fx.hook.close_port(port).expect("closing the opened port");
            format!("{name}=port:{reported}")
        }
        Err(error) => format!("{name}=error:{error}"),
    }
}

/// The oracle's expression for [`probe_names`]: the same label, the same closing of nothing (the
/// VM exits), rendered through `~s` so stdout is the bare comma-joined line.
fn oracle_expression(names: &[&str]) -> String {
    let names = names
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "F = fun(N) -> \
           try open_port({{spawn_driver, N}}, []) of \
             P -> case erlang:port_info(P, name) of \
                    {{name, S}} -> N ++ \"=port:\" ++ S; \
                    O -> N ++ \"=port:\" ++ lists:flatten(io_lib:format(\"~p\", [O])) \
                  end \
           catch error: E -> N ++ \"=error:\" ++ lists:flatten(io_lib:format(\"~p\", [E])) \
           end \
         end, \
         io:format(\"~s\", [lists:join(\",\", [F(N) || N <- [{names}]])])."
    )
}

/// The runtime's whole answer for `names`, comma-joined — the exact string [`oracle_expression`]
/// prints.
fn runtime_answer(names: &[&str]) -> String {
    let mut fx = fixture();
    names
        .iter()
        .map(|name| runtime_probe(&mut fx, name))
        .collect::<Vec<_>>()
        .join(",")
}

/// Compares `names` with the live oracle, or prints the loud skip banner.
fn assert_agrees_with_the_oracle(names: &[&str]) {
    let observed = runtime_answer(names);
    match erl_eval(&oracle_expression(names)) {
        Some(oracle) => assert_eq!(
            observed, oracle,
            "the `{{spawn_driver, Name}}` namespace must answer like the oracle"
        ),
        None => skip_oracle_check("the `{spawn_driver, Name}` namespace"),
    }
}

/// A **static** emulator driver name opens a live port that reports **that** name, and the three
/// refused names are `badarg` — asserted on this runtime alone, so the namespace is pinned even
/// without an oracle on `PATH`.
#[test]
fn the_static_names_open_and_the_rest_are_badarg() {
    let mut fx = fixture();
    for name in driver::static_drivers() {
        let command = fx.string(name);
        let spec = fx.name("spawn_driver", command);
        let port = fx
            .hook
            .open_port(spec, NIL)
            .unwrap_or_else(|error| panic!("open_port({{spawn_driver,{name}}}) failed: {error}"));
        assert!(fx.registry.borrow().is_live(port), "{name} is live");
        // The reported name is the driver name itself — the oracle's `{name, Name}` — so a port
        // opened as `tcp_inet` can never claim to be another driver.
        assert_eq!(text_of(fx.item(port, "name")), name);
        // It is a **built-in** port: its bytes arrive on the port-task round, not through a C
        // driver's inline `output` callback, and it owns no OS child.
        let kind = fx.registry.borrow().meta.get(&port.number()).map(|meta| meta.kind);
        assert_eq!(kind, Some(PortName::SpawnDriverBuiltin), "{name}");
        assert!(is_atom(fx.item(port, "os_pid")), "{name} owns no OS child");
        fx.hook.close_port(port).expect("a live port closes");
    }
    for name in REFUSED {
        let command = fx.string(name);
        let spec = fx.name("spawn_driver", command);
        assert_badarg(fx.hook.open_port(spec, NIL));
    }
}

/// A **refused** name never reaches the driver factories, so it consumes no port number: after all
/// three refusals the first successful open still gets the registry's first id.
///
/// This is what makes the namespace gate a *validation* rather than a late failure — the closed port
/// numbers of a running node are a resource, and the C rejects the name before it allocates one.
#[test]
fn a_refused_name_consumes_no_port_number() {
    let mut fx = fixture();
    for name in REFUSED {
        let command = fx.string(name);
        let spec = fx.name("spawn_driver", command);
        assert_badarg(fx.hook.open_port(spec, NIL));
    }
    let command = fx.string("ram_file_drv");
    let spec = fx.name("spawn_driver", command);
    let port = fx.hook.open_port(spec, NIL).expect("a static name opens");
    assert_eq!(port, PortId::new(0), "no refusal consumed an id");
}

/// The arrangement layered over `beam_io`'s set is a **permutation** of it: the `erl_ddll` surface
/// cannot list a driver `open_port` refuses, nor omit one it opens.
///
/// This is the property that keeps the two views from forking again — `beam_io` owns the
/// membership, and the local arrangement may only order it.
#[test]
fn the_reported_statics_are_a_permutation_of_beam_ios_set() {
    let reported = driver::static_drivers();
    assert_eq!(
        reported.len(),
        beam_io::STATIC_SPAWN_DRIVERS.len(),
        "one reported name per emulator driver: {reported:?}"
    );
    for name in beam_io::STATIC_SPAWN_DRIVERS {
        assert!(
            reported.contains(&name),
            "{name} is an emulator driver, so it must be reported: {reported:?}"
        );
        assert!(driver::is_static_driver(name), "{name} passes the gate");
    }
    let mut sorted = reported.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), reported.len(), "no duplicates: {reported:?}");
    for name in REFUSED {
        assert!(
            !driver::is_static_driver(name),
            "{name} is not an emulator driver"
        );
    }
}

/// The whole namespace — the five names plus an invented one — agrees with the live oracle:
/// a port reporting its own name for the statics, `error:badarg` for everything else.
///
/// This is the suite's primary evidence: the same expression runs under both runtimes and the
/// rendered lines must match character for character.
#[test]
fn the_namespace_agrees_with_the_erl_oracle() {
    assert_agrees_with_the_oracle(&probe_names());
}

/// `erl_ddll:loaded_drivers/0`, **through the runtime's own BIF dispatch**, is exactly the oracle's
/// `{ok,["tcp_inet","udp_inet","ram_file_drv"]}` — including the order, which that surface is
/// sensitive to (it is the generated `driver_tab.c` order, not a sorted one).
///
/// The list is compared as the oracle renders it, so a name that merely moved fails here; the
/// rendering is built from the returned terms rather than from
/// [`driver::static_drivers`], so it observes the BIF's real answer.
#[test]
fn loaded_drivers_agrees_with_the_oracle_in_order() {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    let answer = {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        context
            .call_bif("erl_ddll", "loaded_drivers", 0, &[])
            .expect("erl_ddll:loaded_drivers/0")
    };
    assert_eq!(
        terms::tuple_arity(answer).expect("a 2-tuple"),
        2,
        "the answer is `{{ok, Names}}`"
    );
    let names = terms::list_elements(terms::tuple_element(answer, 1).expect("the name list"))
        .expect("a proper list")
        .iter()
        .map(|name| {
            terms::list_elements(*name)
                .expect("each name is a charlist")
                .into_iter()
                .map(|byte| char::from(u8::try_from(signed_value(byte)).expect("a byte")))
                .collect::<String>()
        })
        .collect::<Vec<String>>();
    // The statics first, in the emulator's table order.
    let statics: Vec<String> = driver::static_drivers()
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(
        &names[..statics.len()],
        statics.as_slice(),
        "the statics lead the list, in the emulator's table order"
    );
    // ...and the whole rendering matches the oracle's, order included.
    let observed = names.join(",");
    match erl_eval(
        "{ok, Ns} = erl_ddll:loaded_drivers(), \
         io:format(\"~s\", [lists:join(\",\", Ns)]).",
    ) {
        Some(oracle) => assert_eq!(
            observed, oracle,
            "`erl_ddll:loaded_drivers/0` must agree with the oracle, in order"
        ),
        None => skip_oracle_check("`erl_ddll:loaded_drivers/0`"),
    }
}
