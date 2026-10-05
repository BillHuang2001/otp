//! Contract **A.4** — the observable driver / port surface.
//!
//! The driver **C ABI** (`driver_*` / `erl_drv_*`) is not reachable through the
//! black-box CLI: a driver is a C shared object (or built-in C driver) linked into the
//! emulator and reached through a port. What *is* observable is the port machinery that
//! sits on top of it — a port backed by the built-in `spawn` C driver can be opened,
//! reports itself as a port, and its output can be read back into Erlang. These cases
//! pin that surface exactly.
//!
//! The structural half of A.4 (that a C translation unit referencing the `driver_*` /
//! `erl_drv_*` declarations still compiles against the frozen headers) is asserted in
//! `tests/abi.rs` — never fabricated here.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.4 cases: the port/driver subsystem as observed from Erlang.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "driver/os_cmd_roundtrip",
            description: "os:cmd/1 runs through the spawn driver and its output reads back",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "byte_size(list_to_binary(os:cmd(\"echo hi\")))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `os:cmd/1` returns "hi\n" (three bytes) by way of the spawn driver
                // and the port output path.
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_backed_by_spawn_driver",
            description: "open_port/2 through the spawn driver yields a real port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "is_port(open_port({spawn,\"true\"},[]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
    ]
}
