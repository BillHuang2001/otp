//! Contract **A.8** — introspection: `erlang:system_info/1` and `erlang:process_info/2`.
//!
//! Volatile keys (process count, OTP release string, heap sizes, reductions) are asserted
//! **structurally** rather than by exact value, so the case stays true across releases: what
//! matters is that the key exists and has the documented type. `version` (and the other
//! frozen build constants) are asserted exactly because they are the frozen target of the
//! rewrite (erts 17.1).
//!
//! # Why the exact values below are safe
//!
//! Each pinned value is a **build constant of the frozen target** — the same on any host,
//! because it is what both runtimes must report for `erts-17.1` (`atom_limit`, `port_limit`,
//! `ets_limit`, `dist_buf_busy_limit`, `delayed_node_table_gc`, `nif_version`,
//! `driver_version`, `machine`, `os_type`, `heap_type`) — or a **per-process field whose
//! default is a constant** (`fullsweep_after` 65535, `message_queue_data` `on_heap`,
//! `priority` `normal`, `min_bin_vheap_size`). Every value was taken from the **system
//! oracle** (OTP 29.1.1 / `erts-17.1`) with `differential run <id>`.
//!
//! Keys whose value is a per-VM/run-time figure are deliberately **never** pinned exactly
//! here: `process_count`, `atom_count`, `port_count`, `ets_count`, `reductions`,
//! `total_heap_size`, `stack_size`, `memory`, `creation`, `logical_processors` and the
//! `dirty_*` scheduler counts all differ from the oracle's by construction (this runtime's
//! own tables are smaller) or vary per VM start — they are handoffs, not cases.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.8 case: evaluate `expr` under both runtimes and pin the oracle's `~w` rendering.
fn case(id: &'static str, description: &'static str, expr: &'static str, golden: &'static str) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::Introspection],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.8 cases: `system_info/1` keys with stable, structural expectations.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "system_info/version",
            description: "erlang:system_info(version) reports the frozen erts version 17.1",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "list_to_binary(erlang:system_info(version))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<49,55,46,49>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/process_count_is_integer",
            description: "erlang:system_info(process_count) is an integer (exact value is volatile)",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "is_integer(erlang:system_info(process_count))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/otp_release_is_string",
            description: "erlang:system_info(otp_release) is a string (value is release-dependent)",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "is_list(erlang:system_info(otp_release))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/wordsize_is_integer",
            description: "erlang:system_info(wordsize) is an integer (the word size)",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "is_integer(erlang:system_info(wordsize))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/emu_flavor_is_atom",
            description: "erlang:system_info(emu_flavor) is an atom (jit/emu)",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "is_atom(erlang:system_info(emu_flavor))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/schedulers_is_integer",
            description: "erlang:system_info(schedulers) is an integer count",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "is_integer(erlang:system_info(schedulers))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/logical_processors_is_integer",
            description: "erlang:system_info(logical_processors) is an integer count",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "is_integer(erlang:system_info(logical_processors))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/system_architecture_is_string",
            description: "erlang:system_info(system_architecture) is a string (value is machine-dependent)",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "is_list(erlang:system_info(system_architecture))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "system_info/unknown_key_is_badarg",
            description: "erlang:system_info/1 with an unknown key raises error:badarg",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(no_such_key_at_all)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- the frozen build constants (exact values, identical on any host) --------------
        case(
            "system_info/atom_limit",
            "erlang:system_info(atom_limit) is the frozen atom-table ceiling",
            "erlang:system_info(atom_limit)",
            "ok 1048576",
        ),
        case(
            "system_info/port_limit",
            "erlang:system_info(port_limit) is the default port ceiling (no +Q given)",
            "erlang:system_info(port_limit)",
            "ok 65536",
        ),
        case(
            "system_info/process_limit",
            "erlang:system_info(process_limit) is the default process ceiling (no +P given)",
            "erlang:system_info(process_limit)",
            "ok 1048576",
        ),
        case(
            "system_info/ets_limit",
            "erlang:system_info(ets_limit) is the frozen ETS table ceiling",
            "erlang:system_info(ets_limit)",
            "ok 8192",
        ),
        case(
            "system_info/outstanding_system_requests_limit",
            "erlang:system_info(outstanding_system_requests_limit) is the frozen backlog limit",
            "erlang:system_info(outstanding_system_requests_limit)",
            "ok 32",
        ),
        case(
            "system_info/dist_buf_busy_limit",
            "erlang:system_info(dist_buf_busy_limit) is the frozen distribution buffer limit",
            "erlang:system_info(dist_buf_busy_limit)",
            "ok 1048576",
        ),
        case(
            "system_info/delayed_node_table_gc",
            "erlang:system_info(delayed_node_table_gc) is the frozen delay in seconds",
            "erlang:system_info(delayed_node_table_gc)",
            "ok 60",
        ),
        case(
            "system_info/nif_version",
            "erlang:system_info(nif_version) is the frozen NIF ABI version string \"2.18\"",
            "erlang:system_info(nif_version)",
            "ok [50,46,49,56]",
        ),
        case(
            "system_info/driver_version",
            "erlang:system_info(driver_version) is the frozen driver ABI version string \"3.3\"",
            "erlang:system_info(driver_version)",
            "ok [51,46,51]",
        ),
        case(
            "system_info/machine",
            "erlang:system_info(machine) is the frozen emulator identity \"BEAM\"",
            "erlang:system_info(machine)",
            "ok [66,69,65,77]",
        ),
        case(
            "system_info/os_type",
            "erlang:system_info(os_type) is the {unix,_} family tuple",
            "erlang:system_info(os_type)",
            "ok {unix,linux}",
        ),
        case(
            "system_info/heap_type",
            "erlang:system_info(heap_type) is the private-heap family",
            "erlang:system_info(heap_type)",
            "ok private",
        ),
        case(
            "system_info/threads",
            "erlang:system_info(threads) reports a thread-based runtime",
            "erlang:system_info(threads)",
            "ok true",
        ),
        case(
            "system_info/smp_support",
            "erlang:system_info(smp_support) reports a multi-scheduler-capable build",
            "erlang:system_info(smp_support)",
            "ok true",
        ),
        case(
            "system_info/emu_flavor_value",
            "erlang:system_info(emu_flavor) is the atom jit for both runtimes",
            "erlang:system_info(emu_flavor)",
            "ok jit",
        ),
        // --- process_info/2: the items whose value is a documented per-process default -----
        case(
            "system_info/process_info_message_queue_len",
            "process_info(self(), message_queue_len) is 0 for a process that received nothing",
            "erlang:process_info(self(), message_queue_len)",
            "ok {message_queue_len,0}",
        ),
        case(
            "system_info/process_info_messages",
            "process_info(self(), messages) is the empty mailbox",
            "erlang:process_info(self(), messages)",
            "ok {messages,[]}",
        ),
        case(
            "system_info/process_info_priority",
            "process_info(self(), priority) is normal by default",
            "erlang:process_info(self(), priority)",
            "ok {priority,normal}",
        ),
        case(
            "system_info/process_info_dictionary",
            "process_info(self(), dictionary) is the empty process dictionary",
            "erlang:process_info(self(), dictionary)",
            "ok {dictionary,[]}",
        ),
        case(
            "system_info/process_info_monitors",
            "process_info(self(), monitors) has no outgoing monitors for a fresh process",
            "erlang:process_info(self(), monitors)",
            "ok {monitors,[]}",
        ),
        case(
            "system_info/process_info_monitored_by",
            "process_info(self(), monitored_by) has no incoming monitors for a fresh process",
            "erlang:process_info(self(), monitored_by)",
            "ok {monitored_by,[]}",
        ),
        case(
            "system_info/process_info_error_handler",
            "process_info(self(), error_handler) is the default error_handler module",
            "erlang:process_info(self(), error_handler)",
            "ok {error_handler,error_handler}",
        ),
        case(
            "system_info/process_info_message_queue_data",
            "process_info(self(), message_queue_data) is the on_heap default",
            "erlang:process_info(self(), message_queue_data)",
            "ok {message_queue_data,on_heap}",
        ),
        case(
            "system_info/process_info_fullsweep_after",
            "process_info(self(), fullsweep_after) is the 65535 default",
            "erlang:process_info(self(), fullsweep_after)",
            "ok {fullsweep_after,65535}",
        ),
        case(
            "system_info/process_info_min_bin_vheap_size",
            "process_info(self(), min_bin_vheap_size) is the frozen virtual-binary-heap default",
            "erlang:process_info(self(), min_bin_vheap_size)",
            "ok {min_bin_vheap_size,46422}",
        ),
        case(
            "system_info/process_info_trace",
            "process_info(self(), trace) is 0 (tracing off) for a fresh process",
            "erlang:process_info(self(), trace)",
            "ok {trace,0}",
        ),
        case(
            "system_info/process_info_suspending",
            "process_info(self(), suspending) lists no suspenders for a fresh process",
            "erlang:process_info(self(), suspending)",
            "ok {suspending,[]}",
        ),
        case(
            "system_info/process_info_last_calls",
            "process_info(self(), last_calls) is false: call tracing is off",
            "erlang:process_info(self(), last_calls)",
            "ok {last_calls,false}",
        ),
        case(
            "system_info/process_info_memory_is_proplist",
            "process_info(self(), memory) is a memory proplist (its values are volatile)",
            "element(1, erlang:process_info(self(), memory))",
            "ok memory",
        ),
    ]
}
