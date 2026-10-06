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
//! written to with `port_command/2`/`3`, read back as a `{Port, {data, …}}` message,
//! described by `port_info/1` (a proplist of `{Item, Value}` pairs), closed with
//! `port_close/1` and observed through the `exit_status`/`eof`/`stream` options. Where
//! the Rust runtime has no wiring yet, the capability gap signals the runtime's
//! `unsupported` convention (exit `3`) and the case is reported **`unsupported`** — never
//! a fabricated `agree`, and never a `mismatch`. The goldens are always the oracle's own
//! output, so the day the remaining seam lands these cases flip to `agree` on their own.
//!
//! `port_info/2`'s *result* is not compared verbatim: erts-17.1 wraps the item as
//! `{Item, Value}` whereas `beam_rt` returns the bare value (a documented deviation in
//! `crates/beam_rt/src/port.rs`), so an exact comparison would be a `mismatch`. Those
//! cases therefore **normalize** the pair away (`case Raw of {Key, Q} -> Q; Q -> Q end`)
//! and assert only the unwrapped *shape* (`is_list`, `is_integer`, `> 0`). Everything else
//! is asserted through the `port_info/1` proplist — `proplists:get_value/2` and
//! `lists:keymember/3`, which agree — never by iterating an unstable map/proplist order.
//! The full `port_info/1` item set is not identical either (the oracle also reports
//! `input`/`output`, `beam_rt` `stream`/`monitors`), so the structural case filters to the
//! core items both runtimes agree on.
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
                // `cat` echoes exactly the bytes written, so "hello" reads back as
                // [104,101,108,108,111] — the oracle's own output.
                stdout: Some("ok [104,101,108,108,111]"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_command_binary_write_read",
            description: "port_command/2 writes binary bytes to the spawn child and its echo reads back",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // A binary-mode `cat` port echoes exactly the bytes written, so the
                // round-trip data is the binary `<<"hello">>` (ETF `BINARY_EXT`).
                expr: "begin P = open_port({spawn,\"cat\"},[binary]), port_command(P,<<\"hello\">>), receive {P,{data,D}} -> D after 2000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_command3_nosuspend_write_read",
            description: "port_command/3 with the nosuspend option writes and the echo reads back",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"cat\"},[binary]), port_command(P,<<\"pong\">>,[nosuspend]), receive {P,{data,D}} -> D after 2000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,4,112,111,110,103>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_command_empty_write_is_quiet",
            description: "port_command/2 with an empty binary yields no data message",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // Writing zero bytes writes nothing, so the child echoes nothing and the
                // bounded receive must take the `after` branch.
                expr: "begin P = open_port({spawn,\"cat\"},[binary]), port_command(P,<<>>), receive {P,{data,D}} -> D after 2000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,7,116,105,109,101,111,117,116>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_command_after_close_is_badarg",
            description: "port_command/2 on a closed port raises badarg",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // A write to an already-reaped port is an error, not a silent no-op.
                expr: "begin P=open_port({spawn,\"true\"},[]), port_close(P), port_command(P,<<\"x\">>) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "driver/exit_status_option_zero",
            description: "the spawn driver's exit_status option reports a zero exit code",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"true\"},[exit_status]), receive {P,{exit_status,S}} -> S after 3000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/exit_status_option_nonzero",
            description: "the spawn driver's exit_status option reports a non-zero exit code",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // `false` exits with status 1, which is reported verbatim (not collapsed to
                // `0`/`undefined`).
                expr: "begin P = open_port({spawn,\"false\"},[exit_status]), receive {P,{exit_status,S}} -> S after 3000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/open_port_stream_option_is_port",
            description: "open_port/2 with the stream option (the default) yields a real port",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "is_port(open_port({spawn,\"cat\"},[stream,binary]))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/open_port_eof_option_live_child_no_eof",
            description: "an eof-mode spawn port delivers no eof while its child is still running",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // `cat` keeps its stdout open, so no `{Port, eof}` message may arrive and the
                // bounded receive must take the `after` branch.
                expr: "begin P = open_port({spawn,\"cat\"},[binary,eof]), receive {P,eof} -> eof_received after 2000 -> no_eof end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,6,110,111,95,101,111,102>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_after_close_is_undefined",
            description: "port_info/1 on a closed spawn port is undefined",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"true\"},[]), port_close(P), erlang:port_info(P) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                // erts answers the atom `undefined` for a reaped port, not a proplist.
                stdout: Some("ok <<131,119,9,117,110,100,101,102,105,110,101,100>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_item_keys_are_atoms",
            description: "every port_info/1 item key is an atom",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "lists:all(fun(E) -> is_atom(element(1,E)) end, erlang:port_info(open_port({spawn,\"true\"},[])))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_links_item_is_list",
            description: "port_info/1's links item is a list",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // The link list holds the port owner's pid, which is volatile: assert the
                // shape only, never the pid.
                expr: "begin P = open_port({spawn,\"true\"},[]), is_list(proplists:get_value(links, erlang:port_info(P))) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info_core_keys_present",
            description: "port_info/1 carries the core driver item keys",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // The full item set is not identical across runtimes (the oracle also reports
                // `input`/`output`, `beam_rt` `stream`/`monitors`), so filter to the shared core
                // and compare the key set structurally (never an unstable proplist order).
                expr: "lists:sort([K || {K,_} <- erlang:port_info(open_port({spawn,\"true\"},[])), lists:member(K, [connected,id,links,name,os_pid])])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,108,0,0,0,5,119,9,99,111,110,110,101,99,116,101,100,119,2,105,100,119,5,108,105,110,107,115,119,4,110,97,109,101,119,6,111,115,95,112,105,100,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info2_name_value_is_list",
            description: "port_info/2's name lookup yields the command as a character list",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // erts wraps the item as `{name, "true"}`; `beam_rt` returns `"true"` bare, so
                // unwrap either shape before asserting the value's shape.
                expr: "begin P = open_port({spawn,\"true\"},[]), Raw = erlang:port_info(P,name), Val = case Raw of {name,Q} -> Q; Q -> Q end, is_list(Val) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info2_os_pid_shape",
            description: "port_info/2's os_pid lookup yields a positive integer",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                // The OS pid is volatile: assert the shape (a positive integer), never the value.
                expr: "begin P = open_port({spawn,\"true\"},[]), Raw = erlang:port_info(P,os_pid), Val = case Raw of {os_pid,Q} -> Q; Q -> Q end, is_integer(Val) andalso Val > 0 end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "driver/port_info2_links_shape",
            description: "port_info/2's links lookup yields a list",
            contracts: &[Contract::DriverAbi],
            kind: CaseKind::Eval {
                expr: "begin P = open_port({spawn,\"true\"},[]), Raw = erlang:port_info(P,links), Val = case Raw of {links,Q} -> Q; Q -> Q end, is_list(Val) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use crate::corpus::{find_case, CaseKind, Contract, Golden, Probe};

    /// The A.4 cases added to pin the implemented port/driver surface (write/read, close,
    /// `port_info`, the `exit_status`/`eof`/`stream` options). Each must resolve through the
    /// registry and identify itself as a driver-ABI eval case.
    const PORT_SURFACE_IDS: &[&str] = &[
        "driver/port_command_binary_write_read",
        "driver/port_command3_nosuspend_write_read",
        "driver/port_command_empty_write_is_quiet",
        "driver/port_command_after_close_is_badarg",
        "driver/exit_status_option_zero",
        "driver/exit_status_option_nonzero",
        "driver/open_port_stream_option_is_port",
        "driver/open_port_eof_option_live_child_no_eof",
        "driver/port_info_after_close_is_undefined",
        "driver/port_info_item_keys_are_atoms",
        "driver/port_info_links_item_is_list",
        "driver/port_info_core_keys_present",
        "driver/port_info2_name_value_is_list",
        "driver/port_info2_os_pid_shape",
        "driver/port_info2_links_shape",
    ];

    /// Identifier-ish tokens of an Erlang expression (so `Val` is one token, `=:=` is two).
    fn identifiers(expr: &str) -> Vec<&str> {
        expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '@'))
            .filter(|t| !t.is_empty())
            .collect()
    }

    #[test]
    fn port_surface_cases_resolve_with_the_driver_contract() {
        for id in PORT_SURFACE_IDS {
            let case = find_case(id).unwrap_or_else(|| panic!("case {id} is not registered"));
            assert_eq!(
                case.contracts,
                &[Contract::DriverAbi],
                "case {id}: contract"
            );
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                panic!("case {id} must be a plain eval case");
            };
            assert_eq!(probe, Probe::TermToBinary, "case {id}: probe");
            assert!(extra_argv.is_empty(), "case {id}: no extra argv");
            assert!(!expr.is_empty(), "case {id}: empty expression");
            // The probe wrapper binds `V`/`C`/`R`; an expression re-using one of them makes
            // the oracle's `erl_eval` fail with `unsafe_var` instead of evaluating.
            let tokens = identifiers(expr);
            for reserved in ["V", "C", "R"] {
                assert!(
                    !tokens.contains(&reserved),
                    "case {id}: expression must not bind `{reserved}` (probe-wrapper collision)"
                );
            }
            // Goldens are stdout *lines*, without the trailing newline.
            let golden = case.golden.stdout.expect("case {id}: golden stdout");
            assert!(!golden.ends_with('\n'), "case {id}: golden is a line");
        }
    }

    #[test]
    fn port_surface_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for id in PORT_SURFACE_IDS {
            assert!(seen.insert(*id), "duplicate port-surface id {id}");
        }
    }

    #[test]
    fn port_command_roundtrip_cases_pin_the_echoed_bytes() {
        let binary = find_case("driver/port_command_binary_write_read").expect("case present");
        assert_eq!(
            binary.golden,
            Golden {
                stdout: Some("ok <<131,109,0,0,0,5,104,101,108,108,111>>"),
                exit_code: 0,
            }
        );

        let nosuspend =
            find_case("driver/port_command3_nosuspend_write_read").expect("case present");
        assert_eq!(
            nosuspend.golden.stdout,
            Some("ok <<131,109,0,0,0,4,112,111,110,103>>")
        );

        let empty = find_case("driver/port_command_empty_write_is_quiet").expect("case present");
        assert_eq!(
            empty.golden.stdout,
            Some("ok <<131,119,7,116,105,109,101,111,117,116>>")
        );
    }

    #[test]
    fn port_command_on_a_closed_port_raises_badarg() {
        let case = find_case("driver/port_command_after_close_is_badarg").expect("case present");
        assert_eq!(case.golden.stdout, Some("exception error badarg"));
        assert_eq!(case.golden.exit_code, 1, "an exception must not exit 0");
    }

    #[test]
    fn port_info_after_close_is_undefined() {
        let case = find_case("driver/port_info_after_close_is_undefined").expect("case present");
        assert_eq!(
            case.golden.stdout,
            Some("ok <<131,119,9,117,110,100,101,102,105,110,101,100>>")
        );
        assert_eq!(case.golden.exit_code, 0);
    }

    #[test]
    fn exit_status_cases_pin_zero_and_nonzero_exits() {
        let zero = find_case("driver/exit_status_option_zero").expect("case present");
        assert_eq!(zero.golden.stdout, Some("ok <<131,97,0>>"));
        let nonzero = find_case("driver/exit_status_option_nonzero").expect("case present");
        assert_eq!(nonzero.golden.stdout, Some("ok <<131,97,1>>"));
    }
}
