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

//! The `beam_makeops`-generated `opc[]` dispatch-name roster behind
//! `erts_debug:instructions/0`.
//!
//! **This file is GENERATED DATA — never hand-edit it.** It is emitted by the
//! committed generator `gen_instructions.escript` in this directory, which runs
//! this repository's real `erts/emulator/utils/beam_makeops` over the
//! `FLAVOR=jit` x86 tables and parses the `opc[]` initializer of the generated
//! `beam_opcodes.c`. To regenerate (from the OTP checkout root):
//!
//! ```text
//! escript erts_rust/crates/beam_bifs/src/runtime/gen_instructions.escript
//! ```
//!
//! ## Why this file is long
//!
//! It *is* the table: one `&str` per `beam_makeops` dispatch entry, in `opc[]`
//! index order. There is nothing to factor out and nothing to shorten — the
//! names carry the operand-type suffixes `beam_makeops` derives
//! (`allocate_heap_tIt`, `append_cons_Is`, …), so they are *not* `genop.tab`'s
//! opcode names and cannot be derived from the loader's roster. Splitting the
//! table would only obscure that it is machine output.
//!
//! ## Ground truth
//!
//! `erts/emulator/beam/beam_debug.c:376-393` (`erts_debug_instructions_0`)
//! builds `[opc[I].name || I <- 0..num_instructions-1]` — every row of the
//! table, in dispatch order, with nothing filtered. `num_instructions` is the
//! generated `beam_opcodes.c` constant, which the generator cross-checks
//! against the row count before writing this file.
//!
//! ## OTP-version skew
//!
//! The table describes *this* source tree (OTP 30). A live oracle of a
//! different OTP release differs, because `ops.tab` itself differs; the test
//! module of `super::erts_debug` therefore carries an explicit, documented
//! exception set for the release skew and pins this roster's own length.

/// The `opc[]` roster in dispatch order: `INSTRUCTIONS[N]` is the name of
/// instruction `N`.
pub(crate) const INSTRUCTIONS: &[&str] = &[
    "aligned_label_Lt", "allocate_tt",
    "allocate_heap_tIt", "append_cons_Is",
    "apply_t", "apply_last_tt",
    "badmatch_s", "badrecord_s",
    "bif_bit_size_bjsd", "bif_byte_size_bjsd",
    "bif_element_jssd", "bif_hd_sd",
    "bif_is_eq_exact_Ssd", "bif_is_ge_ssd",
    "bif_is_lt_ssd", "bif_is_map_key_bjssd",
    "bif_is_ne_exact_Ssd", "bif_map_get_jssd",
    "bif_map_size_jsd", "bif_max_ssd",
    "bif_min_ssd", "bif_node_jSd",
    "bif_tuple_size_bjSd", "bs_get_integer2_fStsttd",
    "bs_get_tail_Sdt", "bs_init_writable",
    "bs_set_position_SS", "bs_test_tail2_fSW",
    "build_stacktrace", "call_bif_W",
    "call_bif_mfa_aaI", "call_error_handler",
    "call_light_bif_be", "call_nif_WWW",
    "case_end_s", "catch_yf",
    "catch_end_y", "continue_exit",
    "deallocate_t", "empty_func_line",
    "executable_line_II", "fconv_sl",
    "fload_ql", "fload_Sl",
    "fstore_ld", "func_line_I",
    "get_hd_Sd", "get_list_Sdd",
    "get_record_field_jxcad", "get_record_field_jycad",
    "get_tl_Sd", "get_tuple_element_swap_sPdd",
    "get_two_tuple_elements_sPSS", "i_apply",
    "i_apply_fun", "i_apply_fun_last_t",
    "i_apply_fun_only", "i_apply_last_t",
    "i_apply_only", "i_band_ssjd",
    "i_bif1_sjbd", "i_bif2_ssjbd",
    "i_bif3_sssjbd", "i_bnot_jsd",
    "i_bor_jssd", "i_breakpoint_trampoline",
    "i_bs_create_bin_jItd", "i_bs_get_bin_and_tail_SftStdd",
    "i_bs_get_binary2_Sftstd", "i_bs_get_binary_all2_Sfttd",
    "i_bs_get_float2_Sftstd", "i_bs_get_position_SS",
    "i_bs_get_utf16_Sftd", "i_bs_get_utf8_Sfd",
    "i_bs_match_fS", "i_bs_match_string_SfWM",
    "i_bs_match_test_heap_fSIt", "i_bs_skip_bits2_Ssft",
    "i_bs_skip_utf16_Sft", "i_bs_skip_utf8_Sf",
    "i_bs_start_match3_Stjd", "i_bs_validate_unicode_retract_jsS",
    "i_bsl_ssjd", "i_bsr_ssjd",
    "i_bxor_jssd", "i_call_f",
    "i_call_ext_e", "i_call_ext_last_et",
    "i_call_ext_only_e", "i_call_fun_t",
    "i_call_fun2_atS", "i_call_fun2_FtS",
    "i_call_fun2_last_atSt", "i_call_fun2_last_FtSt",
    "i_call_fun_last_tt", "i_call_last_ft",
    "i_call_on_load_function", "i_call_only_f",
    "i_call_trace_return", "i_create_local_native_record_qdtI",
    "i_create_native_record_jcdtI", "i_debug_breakpoint",
    "i_debug_line_IIt", "i_div_rem_ssjdd",
    "i_fadd_lll", "i_fdiv_lll",
    "i_fmul_lll", "i_fnegate_ll",
    "i_fsub_lll", "i_func_info_IaaI",
    "i_func_label_L", "i_generic_breakpoint",
    "i_get_sd", "i_get_hash_cWd",
    "i_get_local_record_elements_qxI", "i_get_local_record_elements_qyI",
    "i_get_map_element_fSSS", "i_get_map_element_hash_fScWS",
    "i_get_map_elements_fsI", "i_get_record_elements_jxI",
    "i_get_record_elements_jyI", "i_get_tuple_element_sPS",
    "i_hibernate", "i_int_div_jssd",
    "i_is_tagged_tuple_fsAa", "i_is_tagged_tuple_ff_ffsAa",
    "i_is_tuple_fs", "i_is_tuple_of_arity_fsA",
    "i_is_tuple_of_arity_ff_ffsA", "i_jump_on_val_sfWI",
    "i_jump_on_val_snWI", "i_lambda_error_t",
    "i_lambda_trampoline_FfWW", "i_length_jtd",
    "i_length_setup_jts", "i_line_breakpoint_trampoline",
    "i_load_nif", "i_loop_rec_f",
    "i_m_div_jssd", "i_make_fun3_FStt",
    "i_minus_ssjd", "i_move_sd",
    "i_mul_add_jssssd", "i_new_small_map_lit_dtqI",
    "i_nif_padding", "i_perf_counter",
    "i_plus_ssjd", "i_rem_ssjd",
    "i_rem_div_ssjdd", "i_return_to_trace",
    "i_select_tuple_arity_SfI", "i_select_val_bins_sfI",
    "i_select_val_bins_snI", "i_select_val_lins_sfI",
    "i_select_val_lins_snI", "i_test_arity_fsA",
    "i_test_yield", "i_trim_t",
    "i_unary_minus_sjd", "i_update_local_native_record_aqsdtI",
    "i_update_native_record_jsdtI", "i_validate_t",
    "i_yield", "if_end",
    "init_yregs_I", "int_code_end",
    "is_any_native_record_fx", "is_any_native_record_fy",
    "is_atom_fs", "is_binary_fs",
    "is_bitstring_fs", "is_boolean_fs",
    "is_eq_fss", "is_eq_exact_fss",
    "is_float_fs", "is_function_fS",
    "is_function2_fss", "is_ge_fss",
    "is_ge_ge_ffScc", "is_ge_lt_ffScc",
    "is_in_range_ffScc", "is_int_ge_fSc",
    "is_int_in_range_fScc", "is_integer_fs",
    "is_list_fs", "is_lt_fss",
    "is_map_fs", "is_native_record_fxaa",
    "is_native_record_fyaa", "is_ne_fss",
    "is_ne_exact_fss", "is_nil_fS",
    "is_nonempty_list_fS", "is_nonempty_list_get_hd_fSd",
    "is_nonempty_list_get_list_fSdd", "is_nonempty_list_get_tl_fSd",
    "is_number_fs", "is_pid_fs",
    "is_port_fs", "is_record_accessible_fxa",
    "is_record_accessible_fya", "is_reference_fs",
    "jump_f", "label_L",
    "line_I", "load_tuple_ptr_s",
    "loop_rec_end_f", "move_two_words_sdsd",
    "new_map_dtI", "nif_start",
    "node_d", "nofail_bif1_sbd",
    "nofail_bif2_ssbd", "normal_exit",
    "on_load", "put_cons_ss",
    "put_tuple2_SA", "raise_ss",
    "raw_raise", "recv_marker_bind_SS",
    "recv_marker_clear_S", "recv_marker_reserve_S",
    "recv_marker_use_S", "remove_message",
    "return", "return_trace",
    "self_d", "send",
    "set_tuple_element_sSP", "store_cons_Id",
    "swap_dd", "system_limit_body",
    "test_heap_It", "timeout",
    "timeout_locked", "trace_jump_W",
    "try_case_y", "try_case_end_s",
    "try_end_y", "try_end_deallocate_t",
    "update_map_assoc_sdtI", "update_map_exact_sjdtI",
    "update_record_aIsdI", "update_record_in_place_IsdI",
    "wait_locked_f", "wait_timeout_locked_sf",
    "wait_timeout_unlocked_sf", "wait_unlocked_f"
];
