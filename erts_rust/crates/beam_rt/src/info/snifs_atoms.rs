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

//! The atom vocabulary of `erlang:system_info(snifs)`: every module and function name the
//! static-NIF roster can emit.
//!
//! `beam_bifs` owns the roster itself ([`beam_bifs::snifs`], generated from this tree's
//! `bif.tab`); this module owns the **names** so the introspection surface's atom
//! vocabulary stays checkable in one place — [`super::info_atoms`] chains it onto
//! [`super::INFO_ATOMS`], and the boot seed ([`crate::boot::seed_core_atoms`]) interns the
//! result, because [`super::InfoValue::Atom`] resolves a name through the runtime's own
//! atom table and reports [`beam_common::BeamError::TermNotFound`] rather than inventing
//! one.
//!
//! ## Why this is a static table
//!
//! The names are data, not logic: 465 distinct spellings (`erlang`, `abs`, `=:=`, …) that
//! never change shape, and a Rust `const` slice is the form the seed and its completeness
//! test iterate. Splitting it out of `info.rs` keeps that parent to the materialization
//! core (~1 000-line rule); keeping it a *table* rather than a walk over the roster at
//! boot keeps the seed free of `beam_bifs`-shaped work and lets the test below pin the
//! exact set.
//!
//! ## Order and drift
//!
//! The order is the roster's own (`bif.tab` file order, first appearance of a name) —
//! informational only, since interning is name-keyed and order-insensitive.
//! [`tests::the_table_is_exactly_the_rosters_distinct_names`] re-derives the set from
//! [`beam_bifs::snifs`] and requires equality both ways, so a `bif.tab` row added to the
//! roster without regenerating this table is a red test, never a silent `TermNotFound`.

/// Every distinct module/function name of [`beam_bifs::snifs`] (465 names).
pub(crate) const SNIFS_ATOMS: &[&str] = &[
    "erlang", "abs", "adler32", "adler32_combine", "atom_to_list", "binary_to_list", "binary_to_term",
    "crc32", "crc32_combine", "date", "delete_module", "display", "display_string", "element", "erase",
    "exit", "exit_signal", "external_size", "float", "float_to_list", "fun_info", "erts_internal",
    "garbage_collect", "get", "get_keys", "group_leader", "halt", "phash", "phash2", "hd", "integer_to_list",
    "length", "link", "list_to_atom", "list_to_binary", "list_to_float", "list_to_pid", "list_to_port",
    "list_to_ref", "list_to_tuple", "loaded", "localtime", "localtime_to_universaltime", "make_ref",
    "unique_integer", "md5", "md5_init", "md5_update", "md5_final", "module_loaded", "function_exported",
    "monitor_node", "node", "nodes", "now", "monotonic_time", "system_time", "time_offset", "timestamp",
    "open_port", "pid_to_list", "ports", "pre_loaded", "process_flag", "process_info", "processes",
    "put", "register", "registered", "round", "self", "setelement", "size", "spawn", "spawn_link",
    "split_binary", "statistics", "term_to_binary", "term_to_iovec", "throw", "time", "tl", "trunc",
    "tuple_to_list", "universaltime", "universaltime_to_localtime", "unlink", "unregister", "whereis",
    "spawn_opt", "setnode", "dist_get_stat", "dist_ctrl_input_handler", "dist_ctrl_put_data", "dist_ctrl_get_data",
    "dist_ctrl_get_data_notification", "dist_ctrl_get_opt", "dist_ctrl_set_opt", "port_info", "port_call",
    "port_command", "port_control", "port_close", "port_connect", "request_system_task", "check_process_code",
    "map_to_tuple_keys", "term_type", "map_hashmap_children", "time_unit", "perf_counter_unit", "is_system_process",
    "system_check", "scheduler_wall_time", "dirty_process_handle_signals", "create_dist_channel",
    "ets_super_user", "spawn_request", "dist_spawn_request", "no_aux_work_threads", "spawn_request_abandon",
    "erts_literal_area_collector", "release_area_switch", "send_copy_request", "port_set_data", "port_get_data",
    "trace_pattern", "trace", "trace_info", "trace_delivered", "seq_trace", "seq_trace_info", "seq_trace_print",
    "suspend_process", "resume_process", "process_display", "bump_reductions", "math", "cos", "cosh",
    "sin", "sinh", "tan", "tanh", "acos", "acosh", "asin", "asinh", "atan", "atanh", "erf", "erfc",
    "exp", "log", "log2", "log10", "sqrt", "atan2", "pow", "start_timer", "send_after", "cancel_timer",
    "read_timer", "make_tuple", "append_element", "system_flag", "system_info", "system_profile",
    "ref_to_list", "port_to_list", "fun_to_list", "monitor", "demonitor", "is_process_alive", "error",
    "raise", "is_builtin", "and", "or", "xor", "not", ">", ">=", "<", "=<", "=:=", "==", "=/=", "/=",
    "+", "-", "*", "/", "div", "rem", "bor", "band", "bxor", "bsl", "bsr", "bnot", "!", "send", "++",
    "append", "--", "subtract", "is_atom", "is_list", "is_tuple", "is_float", "is_integer", "is_number",
    "is_pid", "is_port", "is_reference", "is_binary", "is_function", "is_record", "match_spec_test",
    "ets", "internal_request_all", "new", "delete", "delete_object", "first", "first_lookup", "is_compiled_ms",
    "lookup", "lookup_element", "info", "last", "last_lookup", "match", "match_object", "member",
    "next", "next_lookup", "prev", "prev_lookup", "insert", "insert_new", "rename", "safe_fixtable",
    "slot", "update_counter", "select", "select_count", "select_reverse", "select_replace", "match_spec_compile",
    "match_spec_run_r", "os", "getenv", "putenv", "unsetenv", "getpid", "perf_counter", "erl_ddll",
    "try_load", "try_unload", "loaded_drivers", "format_error_int", "re", "version", "compile", "run",
    "internal_run", "lists", "reverse", "keymember", "keysearch", "keyfind", "erts_debug", "disassemble",
    "breakpoint", "same", "flat_size", "get_internal_state", "set_internal_state", "dist_ext_to_term",
    "instructions", "interpreter_size", "dirty_cpu", "dirty_io", "dirty", "lcnt_control", "lcnt_collect",
    "lcnt_clear", "hibernate", "error_logger", "warning_map", "get_module_info", "is_boolean", "string",
    "make_fun", "iolist_size", "iolist_to_binary", "list_to_existing_atom", "is_bitstring", "tuple_size",
    "byte_size", "bit_size", "list_to_bitstring", "bitstring_to_list", "update_element", "decode_packet",
    "unicode", "characters_to_binary", "characters_to_list", "bin_is_7bit", "atom_to_binary", "binary_to_atom",
    "binary_to_existing_atom", "net_kernel", "dflag_unicode_io", "give_away", "setopts", "load_nif",
    "call_on_load_function", "finish_after_on_load", "binary_part", "binary", "compile_pattern",
    "matches", "longest_common_prefix", "longest_common_suffix", "at", "part", "list_to_bin", "copy",
    "referenced_byte_size", "encode_unsigned", "decode_unsigned", "nif_error", "prim_file", "internal_name2native",
    "internal_native2name", "internal_normalize_utf8", "is_translatable", "file", "native_name_encoding",
    "check_old_code", "universaltime_to_posixtime", "posixtime_to_universaltime", "dt_put_tag", "dt_get_tag",
    "dt_get_tag_data", "dt_spread_tag", "dt_restore_tag", "dt_prepend_vm_tag_data", "dt_append_vm_tag_data",
    "finish_loading", "insert_element", "delete_element", "integer_to_binary", "float_to_binary",
    "binary_to_float", "io", "printable_range", "inspect", "is_map", "map_size", "maps", "find",
    "from_list", "is_key", "keys", "merge", "remove", "update", "values", "cmp_term", "take", "fun_info_mfa",
    "map_info", "is_process_executing_dirty", "check_dirty_process_code", "purge_module", "split",
    "size_shared", "copy_shared", "has_prepared_code_on_load", "floor", "ceil", "fmod", "set_signal",
    "iolist_to_iovec", "get_dflags", "new_connection", "map_next", "gather_alloc_histograms", "gather_carrier_info",
    "map_get", "is_map_key", "internal_delete_all", "internal_select_delete", "persistent_term",
    "erase_persistent_terms", "atomics_new", "atomics", "add", "add_get", "exchange", "compare_exchange",
    "counters_new", "counters_get", "counters_add", "counters_put", "counters_info", "spawn_system_process",
    "ets_lookup_binary_info", "ets_raw_first", "ets_raw_next", "abort_pending_connection", "get_creation",
    "prepare_loading", "beamfile_chunk", "beamfile_module_md5", "env", "alias", "unalias", "from_keys",
    "binary_to_integer", "list_to_integer", "min", "max", "term_to_string", "code", "coverage_support",
    "get_coverage_mode", "get_coverage", "reset_coverage", "set_coverage_mode", "trace_session_create",
    "trace_session_destroy", "erts_trace_cleaner", "check", "send_trace_clean_signal", "system_monitor",
    "processes_next", "get_debug_info", "erl_debugger", "supported", "instrumentations", "toggle_instrumentations",
    "breakpoints", "notify_breakpoint_hit", "stack_frames", "peek_stack_frame_slot", "xregs_count",
    "peek_xreg", "unaligned_bitstring", "import", "put_new", "records", "get_module", "get_name",
    "get_field_names", "is_exported", "create", "get_definition", "pid_to_binary", "port_to_binary",
    "ref_to_binary",
];

#[cfg(test)]
mod tests {
    use super::SNIFS_ATOMS;

    /// The table is exactly the roster's distinct module/function names — no name missing
    /// (which would make `system_info(snifs)` answer `TermNotFound` for a real row) and no
    /// name extra (which would be an unused atom in the boot seed).
    #[test]
    fn the_table_is_exactly_the_rosters_distinct_names() {
        let mut expected: Vec<&'static str> = Vec::new();
        for snif in beam_bifs::snifs() {
            for name in [snif.module, snif.function] {
                if !expected.contains(&name) {
                    expected.push(name);
                }
            }
        }
        let mut expected = expected;
        expected.sort_unstable();
        let mut table = SNIFS_ATOMS.to_vec();
        table.sort_unstable();
        assert_eq!(table.len(), SNIFS_ATOMS.len(), "the table holds no duplicate name");
        assert_eq!(table, expected);
    }

    /// A name an `InfoValue::Atom` can emit must be a legal atom here, i.e. non-empty —
    /// the seed interns these verbatim.
    #[test]
    fn no_name_is_empty() {
        for name in SNIFS_ATOMS {
            assert!(!name.is_empty());
        }
    }
}
