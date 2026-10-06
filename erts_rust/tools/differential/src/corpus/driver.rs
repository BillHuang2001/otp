//! Contract **A.4** — the observable driver / port surface.
//!
//! The driver **C ABI** (`driver_*` / `erl_drv_*`) is not reachable through the
//! black-box CLI: a driver is a C shared object (or built-in C driver) linked into the
//! emulator and reached through a port. What *is* observable is the port machinery that
//! sits on top of it — a port backed by the built-in `spawn` C driver can be opened,
//! reports itself as a port, answers `port_info/1` and its output can be read back into
//! Erlang as a `{Port, {data, …}}` message.
//!
//! Some of these cases exercise the seam end to end (`os:cmd/1` runs a real external
//! program through the `spawn` driver and reads its bytes back; a parsed port term is a
//! port handle). Others drive `open_port/2` and its companions directly: the `spawn`
//! driver starts a real OS process, so the port can be opened in `binary`/`list` mode,
//! described by `port_info/1` (a proplist of `{Item, Value}` pairs) and its stream read
//! back. Where the Rust runtime has no wiring yet (e.g. `port_command/2`, which would
//! write to the child's stdin), the capability gap signals the runtime's `unsupported`
//! convention (exit `3`) and the case is reported **`unsupported`** — never a fabricated
//! `agree`, and never a `mismatch`. The goldens are always the oracle's own output, so
//! the day the remaining seam lands these cases flip to `agree` on their own.
//!
//! `port_info/2` is deliberately **not** probed for exact values: erts-17.1 wraps each
//! item as `{Item, Value}` whereas `beam_rt` returns the bare value (a documented
//! deviation in `crates/beam_rt/src/port.rs`), so an exact comparison would be a
//! `mismatch`. The equivalent facts are asserted through the `port_info/1` proplist —
//! `proplists:get_value/2` and `lists:keymember/3`, which agree — and structurally
//! (`is_integer`, key presence), never by iterating an unstable map/proplist order.
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
            id: "driver/open_port_binary_option_is_port",
            description: "open_port/2 with the binary option still yields a real port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "is_port(open_port({spawn,\"cat\"},[binary]))",
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
                // `port_close/1` kills and reaps the child and answers `true` (the oracle's
                // `erts_internal:port_close/1` value, not `ok`).
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
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/spawn_driver_port_stream_read",
            description: "the spawn driver's port stream is read back as a binary message",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"echo hi\"}, [binary]), receive {P,{data,D}} -> D after 500 -> no_data end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `echo hi` writes "hi\n" in one chunk, so the port yields a single data message.
                stdout: Some("ok <<104,105,10>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/spawn_driver_binary_stream_printf_read",
            description: "a binary-mode spawn port reads a finite producer's exact bytes",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"printf abc\"}, [binary]), receive {P,{data,D}} -> D after 1000 -> no_data end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `printf abc` writes three bytes and no newline: one binary data message.
                stdout: Some("ok <<97,98,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/spawn_driver_list_stream_read",
            description: "a list-mode spawn port delivers its stream as a character list",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"echo hi\"}, []), receive {P,{data,D}} -> D after 500 -> no_data end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // The default stream type is `list`, so "hi\n" arrives as [104,105,10].
                stdout: Some("ok [104,105,10]"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/spawn_driver_binary_message_is_binary",
            description: "a binary-mode spawn port delivers a binary, not a char list",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"echo hi\"}, [binary]), receive {P,{data,D}} -> is_binary(D) after 500 -> no_data end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_stream_read_without_output_is_no_data",
            description: "a quiet spawn port yields no data message and the after clause fires",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // `cat` reads stdin forever and writes nothing until `port_command/2` is used,
                // so the bounded `receive … after` must take the fallback branch.
                expr: "begin P = open_port({spawn,\"cat\"}, [binary]), receive {P,{data,D}} -> D after 1000 -> no_data end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok no_data"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_items_are_pairs",
            description: "every element of port_info/1 is an {Item, Value} 2-tuple",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "lists:all(fun({_,_}) -> true; (_) -> false end, erlang:port_info(open_port({spawn,\"true\"},[])))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_name_item_via_proplist",
            description: "port_info/1's name item is the spawn driver's command string",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "proplists:get_value(name, erlang:port_info(open_port({spawn,\"true\"},[])))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `"true"` as a character list, read out of the proplist by key (never by
                // iterating the list, whose order is not part of the contract).
                stdout: Some("ok [116,114,117,101]"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_os_pid_is_positive_integer",
            description: "port_info/1's os_pid item is a positive integer for a spawned port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // The exact pid is volatile; assert the shape (integer, > 0) instead.
                expr: "begin P = open_port({spawn,\"true\"},[]), Pid = proplists:get_value(os_pid, erlang:port_info(P)), is_integer(Pid) andalso Pid > 0 end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_name_key_present",
            description: "port_info/1 carries a name item for a spawned port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "lists:keymember(name, 1, erlang:port_info(open_port({spawn,\"true\"},[])))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_connected_key_present",
            description: "port_info/1 carries a connected item for a spawned port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "lists:keymember(connected, 1, erlang:port_info(open_port({spawn,\"true\"},[])))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_id_key_present",
            description: "port_info/1 carries an id item for a spawned port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // Only the presence of the key is asserted: erts reports a bare integer id
                // while `beam_rt` reports the port term (a documented deviation).
                expr: "lists:keymember(id, 1, erlang:port_info(open_port({spawn,\"true\"},[])))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/ports_lists_open_port",
            description: "erlang:ports/0 includes a freshly opened spawn port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"true\"},[]), length(erlang:ports()) >= 1 end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_command_write_read",
            description: "port_command/2 writes to the spawn child and its echo reads back",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // `cat` echoes exactly the bytes written, so "hello" reads back as [104,101,108,108,111].
                expr: "begin P = open_port({spawn,\"cat\"}, []), port_command(P, \"hello\"), receive {P,{data,D}} -> D after 1000 -> no_data end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                // `port_command/2` is not wired into the Rust runtime yet: it is an honest
                // capability gap (exit 3), and the golden is the oracle's own output.
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
    ]
}
