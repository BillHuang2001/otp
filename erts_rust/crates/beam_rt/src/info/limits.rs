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

//! The `limits` family of [`super::system_info_value`]: the runtime's table counts plus
//! the emulator's fixed limits.
//!
//! The **counts** (`atom_count`, `process_count`, `port_count`, `ets_count`) are read
//! from *this* runtime's own live tables and are therefore correct-but-smaller than the
//! oracle's run-time figures — that divergence is deliberate and must not be "fixed" to
//! a hardcoded oracle number (the same rule `erlang:processes/0`/`ports/0` follows).
//! The **limits** are the emulator's build constants (verified against OTP 29.1.1 /
//! erts-17.1), with `process_limit` honouring an explicit `+P N` and `port_limit` a
//! `+Q N` / `ERL_MAX_PORTS` request.
//!
//! | key | shape | source |
//! |---|---|---|
//! | `atom_count` | int | this runtime's atom table length |
//! | `atom_limit` | int `1048576` | emulator constant |
//! | `process_count` | int | this runtime's process table length |
//! | `process_limit` | int | `+P N` or `1048576` |
//! | `port_count` | int | this runtime's live port count |
//! | `port_limit` | int | `+Q N`/`ERL_MAX_PORTS` sized up to a power of two, else `65536` |
//! | `ets_count` | int | this runtime's live ETS table count |
//! | `ets_limit` | int `8192` | emulator constant |
//! | `outstanding_system_requests_limit` | int `32` | emulator constant |
//! | `dist_buf_busy_limit` | int `1048576` | emulator constant |
//! | `delayed_node_table_gc` | int `60` | emulator constant |

use beam_common::Result;

use super::{live_ports, live_tables, InfoValue};
use crate::runtime::Runtime;

/// The atom table's limit (`ATOM_LIMIT`, `erl_bif_info.c`).
const ATOM_LIMIT: i64 = 1_048_576;
/// The default process limit (`ERTS_MAX_PROCESSES`).
const PROCESS_LIMIT: i64 = 1_048_576;
/// The **default** port limit — the size of the port table a bare invocation (and
/// `+Q legacy`) gets (`ERTS_DEFAULT_MAX_PORTS`, `erl_port.h:41`).
const PORT_LIMIT: i64 = 65_536;
/// `ERTS_MAX_PORTS = ERTS_PTAB_MAX_SIZE - 1` (`erl_node_container_utils.h:248`) — the
/// hard upper limit of the port table, and therefore the largest `port_limit` the oracle
/// can report (`ERTS_PTAB_MAX_SIZE` = `1 << 27`, `erl_ptab.h:174`).
const ERTS_MAX_PORTS: i64 = 134_217_727;
/// The ETS table limit (`ERTS_MAX_ETS_TABLES`).
const ETS_LIMIT: i64 = 8_192;
/// The outstanding system-request limit (`ERTS_SSI_MAX_REQUESTS`, `erl_bif_info.c`).
const OUTSTANDING_SYSTEM_REQUESTS_LIMIT: i64 = 32;
/// The distribution buffer busy limit (`erts_dist_buf_busy_limit`, 1 MiB).
const DIST_BUF_BUSY_LIMIT: i64 = 1_048_576;
/// The delayed node-table GC interval in seconds (`erts_delayed_node_table_gc`).
const DELAYED_NODE_TABLE_GC: i64 = 60;

/// The `system_info/1` value of a `limits`-family key, or `None` when `key` is not ours.
pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>> {
    match key {
        "atom_count" => Some(Ok(InfoValue::Int(rt.code_server.atoms().len() as i64))),
        "atom_limit" => Some(Ok(InfoValue::Int(ATOM_LIMIT))),
        "process_count" => Some(Ok(InfoValue::Int(rt.processes().len() as i64))),
        "process_limit" => Some(Ok(InfoValue::Int(process_limit(rt)))),
        "port_count" => Some(live_ports(rt).map(InfoValue::Int)),
        "port_limit" => Some(Ok(InfoValue::Int(port_limit(rt)))),
        "ets_count" => Some(live_tables(rt).map(InfoValue::Int)),
        "ets_limit" => Some(Ok(InfoValue::Int(ETS_LIMIT))),
        "outstanding_system_requests_limit" => {
            Some(Ok(InfoValue::Int(OUTSTANDING_SYSTEM_REQUESTS_LIMIT)))
        }
        "dist_buf_busy_limit" => Some(Ok(InfoValue::Int(DIST_BUF_BUSY_LIMIT))),
        "delayed_node_table_gc" => Some(Ok(InfoValue::Int(DELAYED_NODE_TABLE_GC))),
        _ => None,
    }
}

/// The process limit: an explicit `+P N`, else the emulator default `1048576`.
///
/// `+P` is mapped onto [`RuntimeConfig::max_processes`](crate::config::RuntimeConfig) by
/// the CLI; a runtime that never saw `+P` reports the emulator default.
fn process_limit(rt: &Runtime) -> i64 {
    match rt.config().emulator.max_processes {
        Some(limit) => super::usize_to_int(limit),
        None => PROCESS_LIMIT,
    }
}

/// The **port table size** this runtime would have allocated: the requested size the CLI
/// recorded, else the emulator's default `65536`.
///
/// `+Q N` and `ERL_MAX_PORTS` both land in
/// [`RuntimeConfig::max_ports`](crate::config::RuntimeConfig) already clamped into
/// `[1024, 134217727]`; `+Q legacy` and a bare invocation leave it `None`.
fn port_limit(rt: &Runtime) -> i64 {
    match rt.config().max_ports {
        Some(requested) => sized_port_table(requested),
        None => PORT_LIMIT,
    }
}

/// The emulator's port-table sizing of a requested size `n`.
///
/// `erts_ptab_init_table` (`erl_ptab.c:383-388`) rounds the requested size **up to the
/// next power of two** ("A size that is a power of 2 is to prefer performance wise":
/// `bits = erts_fit_in_bits_int32(size-1); size = 1 << bits`) and caps the result at
/// `ERTS_PTAB_MAX_SIZE` (`1 << 27`, `erl_ptab.h:174`); `erts_ptab_max` (`erl_ptab.h:266`)
/// then reports a table at that cap as `ERTS_MAX_PORTS` = `ERTS_PTAB_MAX_SIZE - 1`
/// (`erl_node_container_utils.h:248`). The two together are exactly
/// `min(next_power_of_two(n), ERTS_MAX_PORTS)`.
///
/// Verified against the oracle (OTP 29.1.1 / erts-17.1):
///
/// ```text
/// +Q 1024 → 1024      +Q 1025 → 2048      +Q 2000 → 2048      +Q 3000 → 4096
/// +Q 4096 → 4096      +Q 4097 → 8192      +Q 67108863 → 67108864
/// +Q 67108864 → 67108864                  +Q 134217726 → 134217727
/// +Q 134217727 → 134217727                +Q legacy → 65536
/// ```
fn sized_port_table(requested: usize) -> i64 {
    // `next_power_of_two` panics on overflow, which the clamp below makes unreachable for
    // a value the CLI accepted — but the answer is the cap, never a panic.
    let rounded = requested
        .checked_next_power_of_two()
        .unwrap_or(ERTS_MAX_PORTS as usize);
    ERTS_MAX_PORTS.min(rounded as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::test_support::runtime;

    #[test]
    fn counts_are_self_derived_and_limits_are_constants() {
        let rt = runtime();
        // The runtime always has its root process and the boot atom seed.
        assert!(matches!(
            try_value("process_count", &rt).unwrap().unwrap(),
            InfoValue::Int(count) if count >= 1
        ));
        assert!(matches!(
            try_value("atom_count", &rt).unwrap().unwrap(),
            InfoValue::Int(count) if count > 0
        ));
        // A fresh runtime has opened no port and created no ETS table.
        assert_eq!(
            try_value("port_count", &rt).unwrap().unwrap(),
            InfoValue::Int(0)
        );
        assert_eq!(
            try_value("ets_count", &rt).unwrap().unwrap(),
            InfoValue::Int(0)
        );
        assert_eq!(
            try_value("atom_limit", &rt).unwrap().unwrap(),
            InfoValue::Int(ATOM_LIMIT)
        );
        assert_eq!(
            try_value("process_limit", &rt).unwrap().unwrap(),
            InfoValue::Int(PROCESS_LIMIT)
        );
        assert_eq!(
            try_value("port_limit", &rt).unwrap().unwrap(),
            InfoValue::Int(PORT_LIMIT)
        );
        assert_eq!(
            try_value("ets_limit", &rt).unwrap().unwrap(),
            InfoValue::Int(ETS_LIMIT)
        );
        assert_eq!(
            try_value("outstanding_system_requests_limit", &rt)
                .unwrap()
                .unwrap(),
            InfoValue::Int(32)
        );
        assert_eq!(
            try_value("dist_buf_busy_limit", &rt).unwrap().unwrap(),
            InfoValue::Int(1_048_576)
        );
        assert_eq!(
            try_value("delayed_node_table_gc", &rt).unwrap().unwrap(),
            InfoValue::Int(60)
        );
        assert!(try_value("nonsense", &rt).is_none());
    }

    #[test]
    fn process_limit_follows_an_explicit_plus_p() {
        let config = crate::config::RuntimeConfig::default().with_max_processes(4096);
        let rt = Runtime::new(config).expect("a runtime with +P");
        assert_eq!(
            try_value("process_limit", &rt).unwrap().unwrap(),
            InfoValue::Int(4096)
        );
    }

    /// The full `+Q`/`ERL_MAX_PORTS` sizing matrix, pinned to the oracle (OTP 29.1.1 /
    /// erts-17.1 — every row was measured with
    /// `erl -noshell +Q <N> -eval 'io:format("~p",[erlang:system_info(port_limit)]), halt(0).'`).
    #[test]
    fn port_limit_follows_an_explicit_plus_q() {
        for (requested, expected) in [
            (1024usize, 1024i64),
            (1025, 2048),
            (2000, 2048),
            (3000, 4096),
            (4096, 4096),
            (4097, 8192),
            (65_536, 65_536),
            (67_108_863, 67_108_864),
            (67_108_864, 67_108_864),
            (134_217_726, 134_217_727),
            (134_217_727, 134_217_727),
        ] {
            let config = crate::config::RuntimeConfig::default().with_max_ports(requested);
            let rt = Runtime::new(config).expect("a runtime with +Q");
            assert_eq!(
                try_value("port_limit", &rt).unwrap().unwrap(),
                InfoValue::Int(expected),
                "+Q {requested}"
            );
        }
    }

    /// The default table is the emulator constant, and it is what `+Q legacy` / no `+Q`
    /// at all reports.
    #[test]
    fn port_limit_defaults_to_the_emulator_table_size() {
        let rt = runtime();
        assert_eq!(rt.config().max_ports, None);
        assert_eq!(
            try_value("port_limit", &rt).unwrap().unwrap(),
            InfoValue::Int(PORT_LIMIT)
        );
        assert_eq!(PORT_LIMIT, 65_536);
    }
}
