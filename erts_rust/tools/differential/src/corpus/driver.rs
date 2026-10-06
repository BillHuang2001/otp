//! Contract **A.4** — the observable driver / port surface.
//!
//! The driver **C ABI** (`driver_*` / `erl_drv_*`) is not reachable through the
//! black-box CLI: a driver is a C shared object (or built-in C driver) linked into the
//! emulator and reached through a port. What *is* observable is the port machinery that
//! sits on top of it — a port backed by the built-in `spawn` C driver can be opened,
//! reports itself as a port, and its output can be read back into Erlang.
//!
//! Some of these cases exercise the seam end to end (`os:cmd/1` runs a real external
//! program through the `spawn` driver and reads its bytes back; a parsed port term is a
//! port handle). Others drive `open_port/2` and its companions directly: when the Rust
//! runtime has no port hook wired up, `open_port/2` is a capability gap that signals the
//! runtime's `unsupported` convention (exit `3`), so the case is reported **`unsupported`**
//! — never a fabricated `agree`, and never a `mismatch`. The goldens are always the
//! oracle's own output, so the day the port seam lands these cases flip to `agree` on
//! their own.
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
        Case {
            id: "driver/os_cmd_printf_output_bytes",
            description: "os:cmd/1 reads the spawn driver's exact output bytes",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "os:cmd(\"printf abc\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `printf abc` writes no trailing newline: the driver returns "abc" only.
                stdout: Some("ok [97,98,99]"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/os_cmd_multiline_output_bytes",
            description: "os:cmd/1 preserves embedded newlines in the driver output",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "os:cmd(\"printf \\\"a\\nb\\\"\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // The output is "a\nb" — the newline is a byte (10), not a line break in the result.
                stdout: Some("ok [97,10,98]"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_term_roundtrip",
            description: "a port handle survives list_to_port/1 -> port_to_list/1 round-tripping",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "erlang:port_to_list(erlang:list_to_port(\"#Port<0.1>\"))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `"#Port<0.1>"` as a character list — the port handle's textual identity.
                stdout: Some("ok [35,80,111,114,116,60,48,46,49,62]"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_term_identity",
            description: "distinct port handles are not equal",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "erlang:list_to_port(\"#Port<0.1>\") =:= erlang:list_to_port(\"#Port<0.2>\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_close_through_spawn_driver",
            description: "port_close/1 closes the port the spawn driver opened",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "erlang:port_close(open_port({spawn,\"true\"},[]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // With no port hook wired up the Rust runtime signals `unsupported` (exit 3).
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_through_spawn_driver",
            description: "port_info/1 describes the port the spawn driver opened",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "is_list(erlang:port_info(open_port({spawn,\"true\"},[])))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // With no port hook wired up the Rust runtime signals `unsupported` (exit 3).
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/spawn_driver_port_stream_read",
            description: "the spawn driver's port stream is read back as a binary message",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"echo hi\"}, [binary]), receive {P,{data,D}} -> D after 500 -> no_data end end",                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `echo hi` writes "hi\n" in one chunk, so the port yields a single data message.
                stdout: Some("ok <<104,105,10>>"),
                exit_code: 0,
            },
        },
    ]
}
