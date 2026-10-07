//! The `limits` family of [`super::system_info_value`]: the runtime's table counts plus
//! the emulator's fixed limits.
//!
//! The **counts** (`atom_count`, `process_count`, `port_count`, `ets_count`) are read
//! from *this* runtime's own live tables and are therefore correct-but-smaller than the
//! oracle's run-time figures — that divergence is deliberate and must not be "fixed" to
//! a hardcoded oracle number (the same rule `erlang:processes/0`/`ports/0` follows).
//! The **limits** are the emulator's build constants (verified against OTP 29.1.1 /
//! erts-17.1), with `process_limit` honouring an explicit `+P N`.
//!
//! | key | shape | source |
//! |---|---|---|
//! | `atom_count` | int | this runtime's atom table length |
//! | `atom_limit` | int `1048576` | emulator constant |
//! | `process_count` | int | this runtime's process table length |
//! | `process_limit` | int | `+P N` or `1048576` |
//! | `port_count` | int | this runtime's live port count |
//! | `port_limit` | int `65536` | emulator constant |
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
/// The port limit (`ERTS_MAX_PORTS`).
const PORT_LIMIT: i64 = 65_536;
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
        "port_limit" => Some(Ok(InfoValue::Int(PORT_LIMIT))),
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
}
