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

//! The drivers' capability word — `ERL_DRV_FLAG_SOFT_BUSY` in the C's `erl_drv_entry`, and the
//! one answer it decides: `erlang:port_command(Port, Data, [force])`.
//!
//! The flag word is per driver entry, not per port state: `erl_bif_port.c:195-198` refuses the
//! `force` option with `error:notsup` for **any** driver whose entry lacks
//! `ERL_DRV_FLAG_SOFT_BUSY`, busy or idle, and with the bit set
//! `erts_port_output` clears its busy mask entirely (`busy_flgs = 0` under
//! `ERTS_PORT_SIG_FLG_FORCE`, `io.c:1731`), so a forced command is never the busy arm.
//!
//! The C truth (`erts/emulator/drivers/common/inet_drv.c`):
//!
//! | entry | flag word | `[force]` |
//! |---|---|---|
//! | `tcp_inet` (`:1374`) | `ERL_DRV_FLAG_USE_PORT_LOCKING\|ERL_DRV_FLAG_SOFT_BUSY` | `true` |
//! | `udp_inet` (`packet_inet`, `:1432`) | `ERL_DRV_FLAG_USE_PORT_LOCKING` only | `error:notsup` |
//! | `ram_file_drv` (`../common/ram_file_drv.c:126`) | `0` | `error:notsup` |
//!
//! Every case drives a port the way the runtime does — through
//! [`PortRegistry::open`](crate::PortRegistry::open) and
//! [`PortRegistry::port_command_with`](crate::PortRegistry::port_command_with), i.e. the real
//! [`Port::command`](crate::Port::command) decision table — and reads the driver's own flag word
//! back through [`PortRegistry::driver_as`](crate::PortRegistry::driver_as), so "the flag is what
//! did it" is asserted rather than assumed.
//!
//! The ports are **socket-less** on purpose: the decision is made from the entry's flag word
//! before any output is attempted, so no socket, no loopback and no wait are needed. The write
//! itself is deliberately not driven here — a forced command that *is* `Done` leaves untagged
//! bytes in the output queue, and a round would run the drivers' `init_caller` (which latches a
//! `DriverFailure` for a payload that is not a `prim_inet` caller tag); that is a different row,
//! pinned by `opcode_tests.rs`'s port-command cases.

use crate::driver::{Driver, DriverFlags};
use crate::drivers::{
    make_ram_file, make_tcp_inet, make_udp_inet, RamFileDriver, TcpInetDriver, UdpInetDriver,
};
use crate::port::{PortCommandOptions, PortCommandOutcome};

use super::opcode_tests::{erl_stdout, reason_of, Rig};

/// The payload every case hands to `port_command/3` — the same bytes the oracle rows use.
const COMMAND: &[u8] = b"abc";

/// The `force` option set (`erlang:port_command(Port, Data, [force])`).
fn force() -> PortCommandOptions {
    PortCommandOptions::EMPTY.with_force(true)
}

/// Our `port_command(Port, COMMAND, [force])` answer in the oracle's own vocabulary: `true` for a
/// queued command (the C's `ERTS_PORT_OP_DONE`), `false` for a busy port (only reachable for a
/// driver that advertises [`DriverFlags::SOFT_BUSY`]), and the raised reason atom otherwise —
/// `notsup` for a driver without the flag, `badarg` for a dead port.
fn forced_command_token(rig: &mut Rig) -> String {
    match rig
        .registry
        .port_command_with(&mut rig.poller, rig.id, COMMAND, force())
    {
        Ok(PortCommandOutcome::Done) => "true".to_string(),
        Ok(PortCommandOutcome::Busy) => "false".to_string(),
        Err(err) => reason_of(err),
    }
}

// ---------------------------------------------------------------------------
// The Rust side: one case per entry
// ---------------------------------------------------------------------------

/// `tcp_inet` advertises `SOFT_BUSY` (the entry's only flag, `inet_drv.c:1374`), so a `[force]`
/// command is `Done`: queued plus an `OUTPUT` task, the oracle's `true`.
///
/// Both halves are asserted, because either could pass on its own for the wrong reason: the flag
/// word read back through the downcast, and the outcome the port's decision table produces from
/// it. The queued bytes are checked too — `Done` means the command really was taken, not that the
/// refuse path answered `Ok`.
#[test]
fn the_tcp_inet_driver_advertises_soft_busy_so_a_forced_command_is_done() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet factory"));

    let flags = rig
        .registry
        .driver_as::<TcpInetDriver>(rig.id)
        .expect("the port's own tcp_inet driver")
        .flags();
    assert!(
        flags.contains(DriverFlags::SOFT_BUSY),
        "the C's `tcp_inet` entry sets `ERL_DRV_FLAG_SOFT_BUSY` (`inet_drv.c:1374`)"
    );
    assert!(
        flags == DriverFlags::SOFT_BUSY,
        "`SOFT_BUSY` is the entry's only flag this crate models"
    );

    let outcome = rig
        .registry
        .port_command_with(&mut rig.poller, rig.id, COMMAND, force())
        .expect("a driver with SOFT_BUSY takes a forced command instead of raising notsup");
    assert_eq!(
        outcome,
        PortCommandOutcome::Done,
        "the oracle answers `true` for `port_command(P, <<\"abc\">>, [force])` on a tcp_inet port"
    );
    assert_eq!(
        rig.registry
            .get(rig.id)
            .expect("the port is live")
            .output_len(),
        COMMAND.len(),
        "`Done` means the command is queued for the scheduled OUTPUT task"
    );
    assert!(
        !rig.registry.is_busy(rig.id).expect("the port is live"),
        "a forced command never leaves the port busy"
    );
}

/// `udp_inet` advertises `ERL_DRV_FLAG_USE_PORT_LOCKING` **only** (`inet_drv.c:1432`), so a
/// `[force]` command is the C BIF's `error:notsup` — the exact reason atom, not `badarg`, and
/// with nothing queued.
#[test]
fn the_udp_inet_driver_keeps_the_empty_flag_word_and_refuses_a_forced_command() {
    let mut rig = Rig::new(make_udp_inet().expect("the udp_inet factory"));

    let flags = rig
        .registry
        .driver_as::<UdpInetDriver>(rig.id)
        .expect("the port's own udp_inet driver")
        .flags();
    assert!(
        flags == DriverFlags::empty(),
        "the C's `packet_inet` entry carries no `ERL_DRV_FLAG_SOFT_BUSY` (`inet_drv.c:1432`)"
    );
    assert!(!flags.contains(DriverFlags::SOFT_BUSY));

    let err = rig
        .registry
        .port_command_with(&mut rig.poller, rig.id, COMMAND, force())
        .expect_err("a driver without SOFT_BUSY refuses a forced command");
    assert_eq!(
        reason_of(err),
        "notsup",
        "`erl_bif_port.c:195-198` raises `notsup` — measured on OTP 29.1.1 for `udp_inet`"
    );
    assert_eq!(
        rig.registry
            .get(rig.id)
            .expect("the port is live")
            .output_len(),
        0,
        "the refusal comes before any output is attempted, so nothing is queued"
    );
}

/// `ram_file_drv` declares the flag word `0` (`../common/ram_file_drv.c:126`), so its `[force]`
/// answer is the same `notsup` — the inverse direction of the `tcp_inet` case, kept separate so
/// neither entry can be "fixed" by analogy with the other.
#[test]
fn the_ram_file_driver_keeps_the_empty_flag_word_and_refuses_a_forced_command() {
    let mut rig = Rig::new(make_ram_file().expect("the ram_file_drv factory"));

    // `RamFileDriver` has an inherent `flags()` (the RAM file's own open flags), so the trait
    // method is named explicitly here rather than resolved through a method call.
    let flags = Driver::flags(
        rig.registry
            .driver_as::<RamFileDriver>(rig.id)
            .expect("the port's own ram_file_drv driver"),
    );
    assert!(
        flags == DriverFlags::empty(),
        "the C's `ram_file_driver_entry` declares `0` as its flag word"
    );

    let err = rig
        .registry
        .port_command_with(&mut rig.poller, rig.id, COMMAND, force())
        .expect_err("a driver without SOFT_BUSY refuses a forced command");
    assert_eq!(
        reason_of(err),
        "notsup",
        "measured on OTP 29.1.1 for `ram_file_drv`"
    );
}

/// A plain command is unaffected by the flag word: every one of the three entries answers
/// `Done` (the oracle's `true`), so what the flag buys is exactly the `[force]` arm.
#[test]
fn a_plain_command_is_done_on_every_entry() {
    for (name, driver) in [
        ("tcp_inet", make_tcp_inet().expect("the tcp_inet factory")),
        ("udp_inet", make_udp_inet().expect("the udp_inet factory")),
        ("ram_file_drv", make_ram_file().expect("the ram_file_drv factory")),
    ] {
        let mut rig = Rig::new(driver);
        assert_eq!(
            rig.registry
                .port_command_with(
                    &mut rig.poller,
                    rig.id,
                    COMMAND,
                    PortCommandOptions::EMPTY
                )
                .unwrap_or_else(|err| panic!("{name}: a plain command must not raise: {err:?}")),
            PortCommandOutcome::Done,
            "{name}: `port_command/2` answers `true` whatever the flag word"
        );
    }
}

// ---------------------------------------------------------------------------
// The oracle row
// ---------------------------------------------------------------------------

/// The `erl` expression that measures all three entries: open a real port on each driver exactly
/// as `prim_inet` does (`open_port({spawn_driver, Name}, [binary])`), issue the `[force]` command
/// under `catch`, close the port, and print the normalized answer per entry (`true` / `false` /
/// the raised reason atom).
///
/// The normalization is the point: `port_command/3` raises, so a raw `~p` of the result would drag
/// a stacktrace into the row. Only [`erl_stdout`]'s "no `erl` / no answer in time" case is a
/// skip — the tool is present and the drivers were really opened.
fn oracle_expression(names: &[&str]) -> String {
    let calls: Vec<String> = names.iter().map(|name| format!("T(\"{name}\")")).collect();
    format!(
        "T = fun(Name) -> \
             P = open_port({{spawn_driver, Name}}, [binary]), \
             V = (catch erlang:port_command(P, <<\"abc\">>, [force])), \
             catch erlang:port_close(P), \
             case V of \
                 true -> \"true\"; \
                 false -> \"false\"; \
                 {{'EXIT', {{R, _}}}} when is_atom(R) -> atom_to_list(R); \
                 R when is_atom(R) -> atom_to_list(R); \
                 _ -> \"other\" \
             end \
         end, \
         io:format(\"FORCE|~s~n\", [lists:flatten(lists:join(\"|\", [{}]))])",
        calls.join(", ")
    )
}

/// The three `[force]` rows against the system drivers — `tcp_inet` `true`, `udp_inet` and
/// `ram_file_drv` `notsup`, which is the whole of what the flag word decides.
#[test]
fn the_forced_command_rows_agree_with_the_erl_oracle() {
    let names = ["tcp_inet", "udp_inet", "ram_file_drv"];
    let Some(stdout) = erl_stdout(&oracle_expression(&names)) else {
        eprintln!("SKIP: no `erl` — the `[force]` rows were not compared");
        return;
    };
    eprintln!("the `[force]` oracle rows: {stdout}");
    let Some(rows) = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("FORCE|"))
    else {
        panic!("the oracle printed no `FORCE|` row line: {stdout:?}");
    };
    let oracle: Vec<&str> = rows.split('|').collect();
    assert_eq!(
        oracle.len(),
        names.len(),
        "one row per driver: {oracle:?}"
    );

    let mut rigs = [
        Rig::new(make_tcp_inet().expect("the tcp_inet factory")),
        Rig::new(make_udp_inet().expect("the udp_inet factory")),
        Rig::new(make_ram_file().expect("the ram_file_drv factory")),
    ];
    for (index, name) in names.iter().enumerate() {
        let ours = forced_command_token(&mut rigs[index]);
        let oracle_row = oracle[index];
        eprintln!("{name}: oracle {oracle_row}, rust {ours}");
        assert_eq!(
            ours, oracle_row,
            "{name}: the `[force]` answer disagrees with the system driver"
        );
    }
}
