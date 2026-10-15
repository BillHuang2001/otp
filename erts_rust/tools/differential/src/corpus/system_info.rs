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
//! own tables are smaller) or vary per VM start. Several of them are pinned instead through a
//! **value-invariant relation** whose answer is host-independent — `process_count =:=
//! length(processes())` and `port_count =:= length(ports())`, a positive-integer
//! `atom_count`, an integer `ets_count`, or a per-process
//! `reductions`/`heap_size`/`stack_size`/`total_heap_size` that is an integer — never by
//! echoing the exact figure. See the *value-invariant introspection* block at the end.
//!
//! # `erlang:memory/0,1` and the tracing surface (still contract A.8)
//!
//! `erlang:memory/0,1` is served by the Rust runtime through its wired `memory` hook, so the
//! five `memory_*` cases **agree** on both sides. Their goldens therefore pin only the
//! host-independent structure — the exact 9-key order, the list length, the selected-key
//! order and the fact that a total is an integer — and never the volatile byte totals. The
//! tracing/`seq_trace` observability and the `halt` argument check below likewise **agree**
//! on both sides and pin the shared surface.
//!
//! # `fun_info`, the wider `system_info` keys, bulk `process_info` and `statistics` (A.8)
//!
//! Several more veins of host-independent introspection, each verified to **agree** with the
//! oracle:
//!
//! * **`erlang:fun_info/1,2`** (ids under the `fun_info/` prefix) — the arity/type/module/name/env
//!   of an *external* fun, a *BIF* fun and a *local fun literal*, in both the `element(2, …)`
//!   extraction form and the `{Key, Value}` tuple form. Only the fields whose answer is a
//!   structural property of the fun are pinned; the identity fields (`uniq`, `new_index`, `index`,
//!   `new_uniq`) and a local fun's `env` are **not** (they legitimately differ). The whole-proplist
//!   form of an *external* fun is pinned by its length and its exact sorted key set.
//! * **the wider `system_info/1` build/state keys** — `update_cpu_info`, `break_ignored`,
//!   `multi_scheduling(_blockers)`, `emu_type`, `lock_checking`, `port_parallelism`,
//!   `scheduler_bind_type`, `min_heap_size`, `min_bin_vheap_size`, `dist`, the `{allocator, …}`
//!   instance lists and `allocated_areas` (list *shape* only — its sizes are volatile). Each is a
//!   frozen build constant, a documented default, or a type assertion.
//! * **`process_info/2` defaults that `self()` cannot show** — `registered_name`, `links` and
//!   `trap_exit` are read from a **freshly spawned** process (unregistered, unlinked, not trapping
//!   exits), and `current_function` / `initial_call` / `current_location` / `catchlevel` are pinned
//!   by their **key tag** only, since each value is a per-run or host-specific figure.
//! * **bulk `process_info/1`** — the no-item-list form returns *a list* whose membership/value for
//!   the documented items is pinned (`priority` `normal`, `message_queue_len`, `total_heap_size`),
//!   never its order or length; plus the tagged `max_heap_size` item.
//! * **`erlang:statistics/1`** — the `runtime` tuple's *shape* (`{Total, SinceLast}`, both integers)
//!   and `run_queue`'s integer type. The clock *values* are volatile and never pinned.

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
        // --- erlang:memory/0,1: the key set/order, never the volatile byte totals ----------
        // The Rust runtime serves these through its wired `memory` hook, so all five **agree**
        // with the oracle. The goldens pin only the host-independent structure (the exact
        // 9-key order, the list length, the selected-key order, an integer total); the volatile
        // byte totals are never pinned.
        case(
            "system_info/memory_keys_order",
            "erlang:memory/0 reports the 9 documented keys in their exact order",
            "[K || {K,_} <- erlang:memory()]",
            "ok [total,processes,processes_used,system,atom,atom_used,binary,code,ets]",
        ),
        case(
            "system_info/memory_length_is_nine",
            "erlang:memory/0 reports exactly nine keys",
            "length(erlang:memory())",
            "ok 9",
        ),
        case(
            "system_info/memory_total_is_integer",
            "erlang:memory(total) is an integer (the exact byte count is volatile)",
            "is_integer(erlang:memory(total))",
            "ok true",
        ),
        case(
            "system_info/memory_selected_keys",
            "erlang:memory/1 keeps the requested key order (the byte values are volatile)",
            "[K || {K,_} <- erlang:memory([atom,total])]",
            "ok [atom,total]",
        ),
        Case {
            id: "system_info/memory_unknown_key_is_badarg",
            description: "erlang:memory/1 with an unknown key raises error:badarg",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:memory(no_such_key)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- the tracing / seq_trace observability surface: AGREE on both runtimes ----------
        case(
            "system_info/dynamic_trace_is_none",
            "erlang:system_info(dynamic_trace) is none in a non-instrumented runtime",
            "erlang:system_info(dynamic_trace)",
            "ok none",
        ),
        case(
            "system_info/trace_control_word_is_zero",
            "erlang:system_info(trace_control_word) is 0 with tracing off",
            "erlang:system_info(trace_control_word)",
            "ok 0",
        ),
        case(
            "system_info/trace_all_flag_accepted",
            "erlang:trace(self(), true, [all]) returns the number of matched processes (1)",
            "erlang:trace(self(),true,[all])",
            "ok 1",
        ),
        case(
            "system_info/trace_call_flag_accepted",
            "erlang:trace(self(), true, [call]) returns the number of matched processes (1)",
            "erlang:trace(self(),true,[call])",
            "ok 1",
        ),
        Case {
            id: "system_info/trace_bad_option_is_badarg",
            description: "erlang:trace/3 with a malformed option raises error:badarg",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:trace(self(),true,[{bad_option}])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        case(
            "system_info/trace_info_flags",
            "trace_info(self(), flags) is the empty flag list for an untraced process",
            "erlang:trace_info(self(),flags)",
            "ok {flags,[]}",
        ),
        case(
            "system_info/trace_info_tracer",
            "trace_info(self(), tracer) is the empty tracer list for an untraced process",
            "erlang:trace_info(self(),tracer)",
            "ok {tracer,[]}",
        ),
        case(
            "system_info/seq_trace_get_token_is_empty",
            "seq_trace:get_token/0 is the empty list when no sequential trace is set",
            "seq_trace:get_token()",
            "ok []",
        ),
        case(
            "system_info/seq_trace_set_label_token",
            "seq_trace:set_token(label, 1) succeeds and returns 0",
            "seq_trace:set_token(label,1)",
            "ok 0",
        ),
        Case {
            id: "system_info/halt_bad_arg_is_badarg",
            description: "erlang:halt/1 with a malformed argument raises error:badarg",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:halt({bad})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- value-invariant introspection: host-independent relations and documented types --
        // Each case pins a *relation* or a documented type whose answer is identical on any
        // host, instead of echoing a per-VM figure (which would differ between the two runtimes
        // by construction). `process_count =:= length(processes())` is the canonical example:
        // the count itself is volatile, the equality is not.
        case(
            "system_info/process_count_matches_processes",
            "system_info(process_count) equals length(processes())",
            "erlang:system_info(process_count) =:= length(erlang:processes())",
            "ok true",
        ),
        case(
            "system_info/port_count_matches_ports",
            "system_info(port_count) equals length(ports())",
            "erlang:system_info(port_count) =:= length(erlang:ports())",
            "ok true",
        ),
        case(
            "system_info/atom_count_is_positive_integer",
            "system_info(atom_count) is a positive integer (the exact figure is volatile)",
            "is_integer(erlang:system_info(atom_count)) andalso erlang:system_info(atom_count) > 0",
            "ok true",
        ),
        case(
            "system_info/ets_count_is_integer",
            "system_info(ets_count) is an integer (the exact figure is volatile)",
            "is_integer(erlang:system_info(ets_count))",
            "ok true",
        ),
        // --- process_info/2: the per-process fields pinned through their documented type -----
        // A figure such as the reduction count or a heap size differs per run-time and per VM
        // start, so only its *type* is pinned; `garbage_collection` is a proplist and `status`
        // is tagged by its own key. `trap_exit`/`registered_name`/`links` are never observed on
        // `self()` — the oracle's `-eval` runs in its `init` process (trap_exit true, registered
        // as `init`, linked to the node), so those values are not host-independent on this path;
        // they are pinned below through a *freshly spawned* process instead.
        case(
            "system_info/process_info_reductions_is_integer",
            "process_info(self(), reductions) is an integer (the exact count is volatile)",
            "is_integer(element(2, erlang:process_info(self(), reductions)))",
            "ok true",
        ),
        case(
            "system_info/process_info_heap_size_is_integer",
            "process_info(self(), heap_size) is an integer (the exact size is volatile)",
            "is_integer(element(2, erlang:process_info(self(), heap_size)))",
            "ok true",
        ),
        case(
            "system_info/process_info_stack_size_is_integer",
            "process_info(self(), stack_size) is an integer (the exact size is volatile)",
            "is_integer(element(2, erlang:process_info(self(), stack_size)))",
            "ok true",
        ),
        case(
            "system_info/process_info_total_heap_size_is_integer",
            "process_info(self(), total_heap_size) is an integer (the exact size is volatile)",
            "is_integer(element(2, erlang:process_info(self(), total_heap_size)))",
            "ok true",
        ),
        case(
            "system_info/process_info_garbage_collection_is_list",
            "process_info(self(), garbage_collection) is a proplist (its fields are volatile)",
            "is_list(element(2, erlang:process_info(self(), garbage_collection)))",
            "ok true",
        ),
        case(
            "system_info/process_info_status_is_tagged",
            "process_info(self(), status) is tagged by its own key",
            "element(1, erlang:process_info(self(), status))",
            "ok status",
        ),
        // --- process_info/2 of a *freshly spawned* process ------------------------------------
        // `self()` cannot expose these: the oracle's `-eval` runs in its `init` process, which
        // traps exits, is registered as `init` and is linked to the node. A plain `spawn/1`
        // process is unregistered, unlinked and does not trap exits — the same answer on both
        // runtimes — so its fields are host-independent. (The spawned process is left parked in
        // a `receive`; the 5000 ms timeout is never reached.)
        case(
            "system_info/process_info_spawned_registered_name",
            "process_info(P, registered_name) of a fresh spawned process is the bare empty list",
            "begin P = spawn(fun() -> receive after 5000 -> ok end end), erlang:process_info(P, registered_name) end",
            "ok []",
        ),
        case(
            "system_info/process_info_spawned_links_length",
            "a fresh spawned process has no links",
            "begin P = spawn(fun() -> receive after 5000 -> ok end end), length(element(2, erlang:process_info(P, links))) end",
            "ok 0",
        ),
        case(
            "system_info/process_info_spawned_trap_exit",
            "a fresh spawned process does not trap exits",
            "begin P = spawn(fun() -> receive after 5000 -> ok end end), erlang:process_info(P, trap_exit) end",
            "ok {trap_exit,false}",
        ),
        // --- process_info/2: the items pinned by their own key tag ----------------------------
        // The *value* of each item below is a per-run figure (the executing function, the start
        // function, a code location, the catch depth), so only its key tag is pinned.
        case(
            "system_info/process_info_current_function",
            "process_info/2 current_function is a tuple keyed by the atom current_function",
            "element(1, erlang:process_info(self(), current_function))",
            "ok current_function",
        ),
        case(
            "system_info/process_info_initial_call",
            "process_info/2 initial_call is a tuple keyed by the atom initial_call",
            "element(1, erlang:process_info(self(), initial_call))",
            "ok initial_call",
        ),
        case(
            "system_info/process_info_current_location",
            "process_info/2 current_location is a tuple keyed by the atom current_location",
            "element(1, erlang:process_info(self(), current_location))",
            "ok current_location",
        ),
        case(
            "system_info/process_info_catchlevel",
            "process_info/2 catchlevel is a tuple keyed by the atom catchlevel",
            "element(1, erlang:process_info(self(), catchlevel))",
            "ok catchlevel",
        ),
        // --- the scheduler / thread introspection keys ($+S / +A observables) ---------------
        // `scheduler_id/0` (the calling scheduler), the dirty-scheduler counts and the
        // async-thread pool size are all per-VM figures, so only their *type* (or a
        // documented relation) is pinned; `schedulers_online =< schedulers` is the canonical
        // value-invariant relation for the pair. Every golden is the oracle's own output.
        case(
            "system_info/scheduler_id_is_integer",
            "system_info(scheduler_id) is an integer (the calling scheduler is per-VM)",
            "is_integer(erlang:system_info(scheduler_id))",
            "ok true",
        ),
        case(
            "system_info/dirty_cpu_schedulers_is_integer",
            "system_info(dirty_cpu_schedulers) is an integer count",
            "is_integer(erlang:system_info(dirty_cpu_schedulers))",
            "ok true",
        ),
        case(
            "system_info/dirty_io_schedulers_is_integer",
            "system_info(dirty_io_schedulers) is an integer count",
            "is_integer(erlang:system_info(dirty_io_schedulers))",
            "ok true",
        ),
        case(
            "system_info/schedulers_online_at_most_schedulers",
            "system_info(schedulers_online) never exceeds system_info(schedulers)",
            "erlang:system_info(schedulers_online) =< erlang:system_info(schedulers)",
            "ok true",
        ),
        case(
            "system_info/thread_pool_size_is_integer",
            "system_info(thread_pool_size) is an integer count",
            "is_integer(erlang:system_info(thread_pool_size))",
            "ok true",
        ),
        case(
            "system_info/garbage_collection_is_a_proplist",
            "system_info(garbage_collection) is the documented per-level proplist",
            "is_list(erlang:system_info(garbage_collection))",
            "ok true",
        ),
        Case {
            id: "system_info/unknown_nonatom_key_is_badarg",
            description: "erlang:system_info/1 with a non-atom key (not just an unknown atom) is error:badarg",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:system_info(42)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        case(
            "system_info/memory_selected_keys_keep_the_requested_order",
            "erlang:memory/1 with a three-key request keeps that order (values are volatile)",
            "[K || {K,_} <- erlang:memory([total,processes,atom])]",
            "ok [total,processes,atom]",
        ),
        case(
            "system_info/process_info_min_heap_size_is_tagged",
            "process_info(self(), min_heap_size) is tagged by its own key",
            "element(1, erlang:process_info(self(), min_heap_size))",
            "ok min_heap_size",
        ),
        Case {
            id: "system_info/process_info_unknown_item_is_badarg",
            description: "erlang:process_info/2 with an unknown item raises error:badarg",
            contracts: &[Contract::Introspection],
            kind: CaseKind::Eval {
                expr: "erlang:process_info(self(), no_such_item)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- the `seq_trace` token surface: the setter's *return* value is the observable ---
        case(
            "system_info/seq_trace_set_token_serial_returns_previous",
            "seq_trace:set_token(serial, _) returns the previous serial token",
            "seq_trace:set_token(serial, {0,1})",
            "ok {0,0}",
        ),
        case(
            "system_info/seq_trace_set_token_print_flag_returns_previous",
            "seq_trace:set_token(print, true) returns the previous print flag",
            "seq_trace:set_token(print, true)",
            "ok false",
        ),
        case(
            "fun_info/external_fun_arity",
            "fun_info/2 arity of an external fun is its declared arity (1)",
            "element(2, erlang:fun_info(fun lists:reverse/1, arity))",
            "ok 1",
        ),
        case(
            "fun_info/external_fun_type",
            "fun_info/2 type of an external fun is the atom external",
            "element(2, erlang:fun_info(fun lists:reverse/1, type))",
            "ok external",
        ),
        case(
            "fun_info/external_fun_module",
            "fun_info/2 module of an external fun names its defining module",
            "element(2, erlang:fun_info(fun lists:reverse/1, module))",
            "ok lists",
        ),
        case(
            "fun_info/external_fun_name",
            "fun_info/2 name of an external fun is its exported function name",
            "element(2, erlang:fun_info(fun lists:reverse/1, name))",
            "ok reverse",
        ),
        case(
            "fun_info/external_fun_env_is_empty",
            "fun_info/2 env of an external fun carries no free variables",
            "element(2, erlang:fun_info(fun lists:reverse/1, env))",
            "ok []",
        ),
        case(
            "fun_info/external_fun_arity_three",
            "fun_info/2 arity reports 3 for an arity-3 external fun",
            "element(2, erlang:fun_info(fun lists:seq/3, arity))",
            "ok 3",
        ),
        case(
            "fun_info/external_fun_module_three",
            "fun_info/2 module of an arity-3 external fun is its defining module",
            "element(2, erlang:fun_info(fun lists:seq/3, module))",
            "ok lists",
        ),
        case(
            "fun_info/external_fun_info_length",
            "erlang:fun_info/1 of an external fun is a 5-element proplist",
            "length(erlang:fun_info(fun lists:reverse/1))",
            "ok 5",
        ),
        case(
            "fun_info/external_fun_info_keys",
            "erlang:fun_info/1 of an external fun carries exactly the arity/env/module/name/type keys",
            "lists:sort([element(1,X) || X <- erlang:fun_info(fun lists:reverse/1)])",
            "ok [arity,env,module,name,type]",
        ),
        case(
            "fun_info/bif_module",
            "fun_info/2 module of a BIF fun is the erlang module",
            "element(2, erlang:fun_info(fun erlang:abs/1, module))",
            "ok erlang",
        ),
        case(
            "fun_info/bif_name",
            "fun_info/2 name of a BIF fun is the BIF name",
            "element(2, erlang:fun_info(fun erlang:abs/1, name))",
            "ok abs",
        ),
        case(
            "fun_info/bif_arity",
            "fun_info/2 arity of a BIF fun is its declared arity",
            "element(2, erlang:fun_info(fun erlang:abs/1, arity))",
            "ok 1",
        ),
        case(
            "fun_info/bif_env_is_empty",
            "fun_info/2 env of a BIF fun carries no free variables",
            "element(2, erlang:fun_info(fun erlang:abs/1, env))",
            "ok []",
        ),
        case(
            "fun_info/bif_type_is_external",
            "fun_info/2 type of a BIF fun is external",
            "element(2, erlang:fun_info(fun erlang:abs/1, type))",
            "ok external",
        ),
        case(
            "fun_info/local_fun_unary_arity",
            "fun_info/2 arity of a local fun literal is its declared arity",
            "element(2, erlang:fun_info(fun (X) -> X + 1 end, arity))",
            "ok 1",
        ),
        case(
            "fun_info/local_fun_unary_type",
            "fun_info/2 type of a local fun literal is the atom local",
            "element(2, erlang:fun_info(fun (X) -> X + 1 end, type))",
            "ok local",
        ),
        case(
            "fun_info/local_fun_binary_arity",
            "fun_info/2 arity of an arity-2 local fun literal is 2",
            "element(2, erlang:fun_info(fun (A,B) -> {A,B} end, arity))",
            "ok 2",
        ),
        case(
            "fun_info/local_fun_binary_type",
            "fun_info/2 type of an arity-2 local fun literal is local",
            "element(2, erlang:fun_info(fun (A,B) -> {A,B} end, type))",
            "ok local",
        ),
        case(
            "fun_info/maps_get_name",
            "fun_info/2 name of the maps:get/2 external fun is get",
            "element(2, erlang:fun_info(fun maps:get/2, name))",
            "ok get",
        ),
        case(
            "fun_info/erlang_system_info_name",
            "fun_info/2 name of the system_info/1 BIF fun is system_info",
            "element(2, erlang:fun_info(fun erlang:system_info/1, name))",
            "ok system_info",
        ),
        case(
            "fun_info/erlang_system_info_module",
            "fun_info/2 module of the system_info/1 BIF fun is erlang",
            "element(2, erlang:fun_info(fun erlang:system_info/1, module))",
            "ok erlang",
        ),
        case(
            "fun_info/tuple_form_arity",
            "fun_info/2 returns an {arity, A} tuple",
            "erlang:fun_info(fun lists:reverse/1, arity)",
            "ok {arity,1}",
        ),
        case(
            "fun_info/tuple_form_type",
            "fun_info/2 returns a {type, external} tuple for an external fun",
            "erlang:fun_info(fun lists:reverse/1, type)",
            "ok {type,external}",
        ),
        case(
            "fun_info/tuple_form_name",
            "fun_info/2 returns a {name, F} tuple",
            "erlang:fun_info(fun lists:reverse/1, name)",
            "ok {name,reverse}",
        ),
        case(
            "fun_info/tuple_form_module",
            "fun_info/2 returns a {module, M} tuple",
            "erlang:fun_info(fun lists:reverse/1, module)",
            "ok {module,lists}",
        ),
        case(
            "fun_info/tuple_form_env",
            "fun_info/2 returns an {env, []} tuple for an external fun",
            "erlang:fun_info(fun lists:reverse/1, env)",
            "ok {env,[]}",
        ),
        case(
            "fun_info/bif_tuple_form_name",
            "fun_info/2 on a BIF fun returns a {name, F} tuple",
            "erlang:fun_info(fun erlang:self/0, name)",
            "ok {name,self}",
        ),
        case(
            "system_info/update_cpu_info",
            "system_info(update_cpu_info) reports the frozen unchanged default",
            "erlang:system_info(update_cpu_info)",
            "ok unchanged",
        ),
        case(
            "system_info/break_ignored",
            "system_info(break_ignored) reports the frozen false default",
            "erlang:system_info(break_ignored)",
            "ok false",
        ),
        case(
            "system_info/multi_scheduling",
            "system_info(multi_scheduling) reports scheduling as enabled on a mult-core-capable build",
            "erlang:system_info(multi_scheduling)",
            "ok enabled",
        ),
        case(
            "system_info/multi_scheduling_blockers",
            "system_info(multi_scheduling_blockers) lists no blockers for an idle VM",
            "erlang:system_info(multi_scheduling_blockers)",
            "ok []",
        ),
        case(
            "system_info/emu_type",
            "system_info(emu_type) is opt for an optimised emulator build",
            "erlang:system_info(emu_type)",
            "ok opt",
        ),
        case(
            "system_info/lock_checking",
            "system_info(lock_checking) reports the runtime lock-checking mode as false",
            "erlang:system_info(lock_checking)",
            "ok false",
        ),
        case(
            "system_info/port_parallelism",
            "system_info(port_parallelism) reports the port-parallelism default as false",
            "erlang:system_info(port_parallelism)",
            "ok false",
        ),
        case(
            "system_info/scheduler_bind_type",
            "system_info(scheduler_bind_type) is unbound without a scheduler-bind flag",
            "erlang:system_info(scheduler_bind_type)",
            "ok unbound",
        ),
        case(
            "system_info/min_heap_size",
            "system_info(min_heap_size) is the frozen default minimum heap size proplist",
            "erlang:system_info(min_heap_size)",
            "ok {min_heap_size,233}",
        ),
        case(
            "system_info/min_bin_vheap_size",
            "system_info(min_bin_vheap_size) is the frozen default virtual-binary-heap proplist",
            "erlang:system_info(min_bin_vheap_size)",
            "ok {min_bin_vheap_size,46422}",
        ),
        case(
            "system_info/dist_is_encoded_nonode",
            "system_info(dist) encodes the non-distributed node identity of an unconnected VM",
            "erlang:system_info(dist)",
            "ok <<61,110,111,100,101,58,39,110,111,110,111,100,101,64,110,111,104,111,115,116,39,10,61,110,111,95,100,105,115,116,114,105,98,117,116,105,111,110,10>>",
        ),
        case(
            "system_info/allocator_binary_info_is_list",
            "system_info({allocator, binary_alloc}) is a list of allocator instances",
            "is_list(erlang:system_info({allocator, binary_alloc}))",
            "ok true",
        ),
        case(
            "system_info/allocator_eheap_info_is_list",
            "system_info({allocator, eheap_alloc}) is a list of allocator instances",
            "is_list(erlang:system_info({allocator, eheap_alloc}))",
            "ok true",
        ),
        case(
            "system_info/allocated_areas_is_list",
            "system_info(allocated_areas) is a list of area entries (its sizes are volatile)",
            "is_list(erlang:system_info(allocated_areas))",
            "ok true",
        ),
        case(
            "system_info/dirty_cpu_schedulers_online_is_integer",
            "system_info(dirty_cpu_schedulers_online) is an integer count",
            "is_integer(erlang:system_info(dirty_cpu_schedulers_online))",
            "ok true",
        ),
        case(
            "system_info/process_info_bulk_returns_list",
            "process_info(self()) with no item list returns the full bulk item list",
            "is_list(erlang:process_info(self()))",
            "ok true",
        ),
        case(
            "system_info/process_info_bulk_has_priority",
            "the bulk process_info(self()) list contains the priority item",
            "lists:keymember(priority, 1, erlang:process_info(self()))",
            "ok true",
        ),
        case(
            "system_info/process_info_bulk_has_message_queue_len",
            "the bulk process_info(self()) list contains the message_queue_len item",
            "lists:keymember(message_queue_len, 1, erlang:process_info(self()))",
            "ok true",
        ),
        case(
            "system_info/process_info_bulk_priority_value",
            "the bulk process_info(self()) list carries priority normal",
            "lists:keyfind(priority, 1, erlang:process_info(self()))",
            "ok {priority,normal}",
        ),
        case(
            "system_info/process_info_bulk_has_total_heap_size",
            "the bulk process_info(self()) list contains the total_heap_size item",
            "lists:keymember(total_heap_size, 1, erlang:process_info(self()))",
            "ok true",
        ),
        case(
            "system_info/process_info_max_heap_size_is_tagged",
            "process_info(self(), max_heap_size) is tagged by its own key",
            "element(1, erlang:process_info(self(), max_heap_size))",
            "ok max_heap_size",
        ),
        case(
            "system_info/statistics_runtime_is_tuple",
            "erlang:statistics(runtime) is a tuple",
            "is_tuple(erlang:statistics(runtime))",
            "ok true",
        ),
        case(
            "system_info/statistics_runtime_is_two_tuple",
            "erlang:statistics(runtime) is a two-element {Total, SinceLast} tuple",
            "tuple_size(erlang:statistics(runtime))",
            "ok 2",
        ),
        case(
            "system_info/statistics_runtime_total_is_integer",
            "the total runtime in erlang:statistics(runtime) is an integer",
            "is_integer(element(1, erlang:statistics(runtime)))",
            "ok true",
        ),
        case(
            "system_info/statistics_run_queue_is_integer",
            "erlang:statistics(run_queue) is an integer count",
            "is_integer(erlang:statistics(run_queue))",
            "ok true",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.8 introspection group: every case is a `value` eval case claiming
    //! the introspection contract, and no case ever pins a per-VM figure exactly — each
    //! volatile value is either a *type* assertion, a documented relation, or a build
    //! constant of the frozen target.
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};

    #[test]
    fn every_case_is_an_introspection_value_probe() {
        for case in group() {
            assert_eq!(case.contracts, &[Contract::Introspection], "case {}", case.id);
            assert!(
                case.id.starts_with("system_info/") || case.id.starts_with("fun_info/"),
                "case {} is outside the documented A.8 introspection prefixes",
                case.id
            );
            assert!(!case.description.is_empty(), "case {}", case.id);
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                panic!("case {}: an A.8 case must be an eval case", case.id);
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(extra_argv.is_empty(), "case {}", case.id);
            assert!(
                !expr.is_empty() && !expr.contains('\n'),
                "case {}: a one-line expression",
                case.id
            );
            assert!(
                case.golden.stdout.is_some_and(|s| !s.is_empty()),
                "case {}: empty golden",
                case.id
            );
        }
    }

    /// The widened scheduler / memory / `process_info` / `seq_trace` block: each case pins
    /// either a type, a documented relation, or a tagged key — never a volatile figure.
    #[test]
    fn the_widened_block_pins_types_relations_and_tags() {
        let golden = |id: &str| {
            group()
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no case {id}"))
                .golden
                .stdout
                .expect("a golden")
        };
        for id in [
            "system_info/scheduler_id_is_integer",
            "system_info/dirty_cpu_schedulers_is_integer",
            "system_info/dirty_io_schedulers_is_integer",
            "system_info/schedulers_online_at_most_schedulers",
            "system_info/thread_pool_size_is_integer",
            "system_info/garbage_collection_is_a_proplist",
        ] {
            assert_eq!(golden(id), "ok true", "case {id}");
        }
        assert_eq!(
            golden("system_info/memory_selected_keys_keep_the_requested_order"),
            "ok [total,processes,atom]"
        );
        assert_eq!(
            golden("system_info/process_info_min_heap_size_is_tagged"),
            "ok min_heap_size"
        );
        // The `process_info/2` items that `self()` cannot show (read from a freshly spawned
        // process) and the key-tag-only items: the *value* is a per-run figure, the tag is not.
        for (id, want) in [
            ("system_info/process_info_spawned_registered_name", "ok []"),
            ("system_info/process_info_spawned_links_length", "ok 0"),
            ("system_info/process_info_spawned_trap_exit", "ok {trap_exit,false}"),
            ("system_info/process_info_current_function", "ok current_function"),
            ("system_info/process_info_initial_call", "ok initial_call"),
            ("system_info/process_info_current_location", "ok current_location"),
            ("system_info/process_info_catchlevel", "ok catchlevel"),
        ] {
            assert_eq!(golden(id), want, "case {id}");
        }
        // The `seq_trace` setters answer the *previous* token, so the goldens are the
        // documented defaults (`{0,0}` / `false`).
        assert_eq!(
            golden("system_info/seq_trace_set_token_serial_returns_previous"),
            "ok {0,0}"
        );
        assert_eq!(
            golden("system_info/seq_trace_set_token_print_flag_returns_previous"),
            "ok false"
        );
        // Both `badarg` cases observe the reason through the probe wrapper (exit 1), never a
        // stack trace: a non-atom `system_info` key and an unknown `process_info` item.
        for (id, expr) in [
            ("system_info/unknown_nonatom_key_is_badarg", "erlang:system_info(42)"),
            (
                "system_info/process_info_unknown_item_is_badarg",
                "erlang:process_info(self(), no_such_item)",
            ),
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.golden.stdout, Some("exception error badarg"), "case {id}");
            assert_eq!(case.golden.exit_code, 1, "case {id}");
            assert_eq!(case.expr(), Some(expr), "case {id}");
        }
    }

    /// The `fun_info/` + extended `system_info`/`process_info`/`statistics` block: each case pins a
    /// structural property of the fun, a frozen build constant, a documented default, or a type —
    /// never a per-VM figure, a fun identity field, a volatile clock value, or a mailbox order.
    #[test]
    fn the_fun_info_and_extended_block_pins_structural_facts() {
        let golden = |id: &str| {
            group()
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no case {id}"))
                .golden
                .stdout
                .expect("a golden")
        };
        // `fun_info`: external / BIF / local funs, extraction and tuple forms.
        for (id, want) in [
            ("fun_info/external_fun_arity", "ok 1"),
            ("fun_info/external_fun_type", "ok external"),
            ("fun_info/external_fun_module", "ok lists"),
            ("fun_info/external_fun_name", "ok reverse"),
            ("fun_info/external_fun_env_is_empty", "ok []"),
            ("fun_info/external_fun_info_length", "ok 5"),
            ("fun_info/external_fun_info_keys", "ok [arity,env,module,name,type]"),
            ("fun_info/bif_module", "ok erlang"),
            ("fun_info/bif_name", "ok abs"),
            ("fun_info/local_fun_unary_arity", "ok 1"),
            ("fun_info/local_fun_unary_type", "ok local"),
            ("fun_info/local_fun_binary_arity", "ok 2"),
            ("fun_info/maps_get_name", "ok get"),
            ("fun_info/tuple_form_arity", "ok {arity,1}"),
            ("fun_info/tuple_form_type", "ok {type,external}"),
            ("fun_info/tuple_form_name", "ok {name,reverse}"),
            ("fun_info/tuple_form_module", "ok {module,lists}"),
            ("fun_info/tuple_form_env", "ok {env,[]}"),
            ("fun_info/bif_tuple_form_name", "ok {name,self}"),
        ] {
            assert_eq!(golden(id), want, "case {id}");
        }
        // The extended `system_info` keys: frozen defaults, documented state, list/integer shapes.
        for (id, want) in [
            ("system_info/update_cpu_info", "ok unchanged"),
            ("system_info/break_ignored", "ok false"),
            ("system_info/multi_scheduling", "ok enabled"),
            ("system_info/multi_scheduling_blockers", "ok []"),
            ("system_info/emu_type", "ok opt"),
            ("system_info/lock_checking", "ok false"),
            ("system_info/port_parallelism", "ok false"),
            ("system_info/scheduler_bind_type", "ok unbound"),
            ("system_info/min_heap_size", "ok {min_heap_size,233}"),
            ("system_info/min_bin_vheap_size", "ok {min_bin_vheap_size,46422}"),
            ("system_info/allocator_binary_info_is_list", "ok true"),
            ("system_info/allocator_eheap_info_is_list", "ok true"),
            ("system_info/allocated_areas_is_list", "ok true"),
            ("system_info/dirty_cpu_schedulers_online_is_integer", "ok true"),
        ] {
            assert_eq!(golden(id), want, "case {id}");
        }
        // Bulk `process_info/1`: membership/value only, never the order or the length.
        for (id, want) in [
            ("system_info/process_info_bulk_returns_list", "ok true"),
            ("system_info/process_info_bulk_has_priority", "ok true"),
            ("system_info/process_info_bulk_has_message_queue_len", "ok true"),
            ("system_info/process_info_bulk_priority_value", "ok {priority,normal}"),
            ("system_info/process_info_bulk_has_total_heap_size", "ok true"),
            ("system_info/process_info_max_heap_size_is_tagged", "ok max_heap_size"),
        ] {
            assert_eq!(golden(id), want, "case {id}");
        }
        // `statistics/1`: shape and type only, never the volatile clock values.
        for (id, want) in [
            ("system_info/statistics_runtime_is_tuple", "ok true"),
            ("system_info/statistics_runtime_is_two_tuple", "ok 2"),
            ("system_info/statistics_runtime_total_is_integer", "ok true"),
            ("system_info/statistics_run_queue_is_integer", "ok true"),
        ] {
            assert_eq!(golden(id), want, "case {id}");
        }
    }
}
