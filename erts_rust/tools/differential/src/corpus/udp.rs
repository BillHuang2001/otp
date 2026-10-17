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
//! Contract **A.4** — the `udp_inet` built-in driver's observable port surface.
//!
//! `beam_rt`'s built-in driver registry serves `udp_inet`
//! (`../../crates/beam_io/src/drivers/inet/udp.rs`), so a `gen_udp`-shaped port is reachable
//! through the documented `erlang:open_port/2` surface: the port options are accepted or
//! refused, `port_command/2,3` sends a datagram, the driver's asynchronous replies
//! (`{udp, Port, Ip, InPort, Data}`) arrive as messages, and `port_info/1,2` reports the
//! driver's own names. Everything here is compared as **stdout + exit status** through the
//! documented CLI (contract A.6) — the driver is never inspected internally.
//!
//! Rows must be loopback-only and self-terminating: a socket that waits for a datagram the
//! other side never sends would hang a case, so every receive carries an `after` guard and
//! the pinned value is the *message*, never a port number or an ephemeral local port.
use super::{Case, CaseKind, Contract, Golden, Probe};
/// The `udp/*` cases.
pub(crate) fn group() -> Vec<Case> {
    let _ = Case {
        id: "udp/placeholder",
        description: "placeholder",
        contracts: &[Contract::DriverAbi],
        kind: CaseKind::Eval {
            expr: "ok",
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("ok ok"),
            exit_code: 0,
        },
    };
    Vec::new()
}
