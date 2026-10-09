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

//! The BEAM generic-opcode roster (`genop.tab`).
//!
//! [`Opcode`] enumerates **every** generic opcode of the wire-frozen roster — the operand
//! tag/length encoding and the instruction stream the compiler emits. This is data derived
//! verbatim from `lib/compiler/src/genop.tab` (the authoritative, checked-in roster); see the
//! "Regeneration" note below. It is complete and lossless so that instructions outside the
//! Milestone-1 interpreter subset still round-trip as `Instr::Generic`.
//!
//! # Regeneration
//!
//! Derived verbatim from `lib/compiler/src/genop.tab` (the wire-frozen generic opcode
//! roster). Regenerate by re-reading that file: every line matching
//! `^<n>: <name>/<arity>` (a leading `-` on the name — a transform-only op — is stripped for
//! the variant identifier but kept in the number/table), in file order. The variant
//! identifier is the op name verbatim (raw-identifier-escaped where it collides with a Rust
//! keyword, e.g. `r#move`); the number is the file's first field.
//!
//! This file is a generated roster: it is intentionally a long, flat table (one variant and
//! four indexed accessors per generic opcode) rather than split across modules.

/// A generic BEAM opcode.
///
/// One variant per entry in `lib/compiler/src/genop.tab`. Variant identifiers are the op
/// names verbatim (non-`CamelCase` on purpose: they mirror the wire names), with Rust
/// keywords escaped (`Opcode::r#move`, `Opcode::r#return`, `Opcode::r#try`).
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Opcode {
    /// `label/1` — generic opcode 1.
    label,
    /// `func_info/3` — generic opcode 2.
    func_info,
    /// `int_code_end/0` — generic opcode 3.
    int_code_end,
    /// `call/2` — generic opcode 4.
    call,
    /// `call_last/3` — generic opcode 5.
    call_last,
    /// `call_only/2` — generic opcode 6.
    call_only,
    /// `call_ext/2` — generic opcode 7.
    call_ext,
    /// `call_ext_last/3` — generic opcode 8.
    call_ext_last,
    /// `bif0/2` — generic opcode 9.
    bif0,
    /// `bif1/4` — generic opcode 10.
    bif1,
    /// `bif2/5` — generic opcode 11.
    bif2,
    /// `allocate/2` — generic opcode 12.
    allocate,
    /// `allocate_heap/3` — generic opcode 13.
    allocate_heap,
    /// `allocate_zero/2` — generic opcode 14.
    allocate_zero,
    /// `allocate_heap_zero/3` — generic opcode 15.
    allocate_heap_zero,
    /// `test_heap/2` — generic opcode 16.
    test_heap,
    /// `init/1` — generic opcode 17.
    init,
    /// `deallocate/1` — generic opcode 18.
    deallocate,
    /// `return/0` — generic opcode 19.
    r#return,
    /// `send/0` — generic opcode 20.
    send,
    /// `remove_message/0` — generic opcode 21.
    remove_message,
    /// `timeout/0` — generic opcode 22.
    timeout,
    /// `loop_rec/2` — generic opcode 23.
    loop_rec,
    /// `loop_rec_end/1` — generic opcode 24.
    loop_rec_end,
    /// `wait/1` — generic opcode 25.
    wait,
    /// `wait_timeout/2` — generic opcode 26.
    wait_timeout,
    /// `m_plus/4` — generic opcode 27.
    m_plus,
    /// `m_minus/4` — generic opcode 28.
    m_minus,
    /// `m_times/4` — generic opcode 29.
    m_times,
    /// `m_div/4` — generic opcode 30.
    m_div,
    /// `int_div/4` — generic opcode 31.
    int_div,
    /// `int_rem/4` — generic opcode 32.
    int_rem,
    /// `int_band/4` — generic opcode 33.
    int_band,
    /// `int_bor/4` — generic opcode 34.
    int_bor,
    /// `int_bxor/4` — generic opcode 35.
    int_bxor,
    /// `int_bsl/4` — generic opcode 36.
    int_bsl,
    /// `int_bsr/4` — generic opcode 37.
    int_bsr,
    /// `int_bnot/3` — generic opcode 38.
    int_bnot,
    /// `is_lt/3` — generic opcode 39.
    is_lt,
    /// `is_ge/3` — generic opcode 40.
    is_ge,
    /// `is_eq/3` — generic opcode 41.
    is_eq,
    /// `is_ne/3` — generic opcode 42.
    is_ne,
    /// `is_eq_exact/3` — generic opcode 43.
    is_eq_exact,
    /// `is_ne_exact/3` — generic opcode 44.
    is_ne_exact,
    /// `is_integer/2` — generic opcode 45.
    is_integer,
    /// `is_float/2` — generic opcode 46.
    is_float,
    /// `is_number/2` — generic opcode 47.
    is_number,
    /// `is_atom/2` — generic opcode 48.
    is_atom,
    /// `is_pid/2` — generic opcode 49.
    is_pid,
    /// `is_reference/2` — generic opcode 50.
    is_reference,
    /// `is_port/2` — generic opcode 51.
    is_port,
    /// `is_nil/2` — generic opcode 52.
    is_nil,
    /// `is_binary/2` — generic opcode 53.
    is_binary,
    /// `is_constant/2` — generic opcode 54.
    is_constant,
    /// `is_list/2` — generic opcode 55.
    is_list,
    /// `is_nonempty_list/2` — generic opcode 56.
    is_nonempty_list,
    /// `is_tuple/2` — generic opcode 57.
    is_tuple,
    /// `test_arity/3` — generic opcode 58.
    test_arity,
    /// `select_val/3` — generic opcode 59.
    select_val,
    /// `select_tuple_arity/3` — generic opcode 60.
    select_tuple_arity,
    /// `jump/1` — generic opcode 61.
    jump,
    /// `catch/2` — generic opcode 62.
    catch,
    /// `catch_end/1` — generic opcode 63.
    catch_end,
    /// `move/2` — generic opcode 64.
    r#move,
    /// `get_list/3` — generic opcode 65.
    get_list,
    /// `get_tuple_element/3` — generic opcode 66.
    get_tuple_element,
    /// `set_tuple_element/3` — generic opcode 67.
    set_tuple_element,
    /// `put_string/3` — generic opcode 68.
    put_string,
    /// `put_list/3` — generic opcode 69.
    put_list,
    /// `put_tuple/2` — generic opcode 70.
    put_tuple,
    /// `put/1` — generic opcode 71.
    put,
    /// `badmatch/1` — generic opcode 72.
    badmatch,
    /// `if_end/0` — generic opcode 73.
    if_end,
    /// `case_end/1` — generic opcode 74.
    case_end,
    /// `call_fun/1` — generic opcode 75.
    call_fun,
    /// `make_fun/3` — generic opcode 76.
    make_fun,
    /// `is_function/2` — generic opcode 77.
    is_function,
    /// `call_ext_only/2` — generic opcode 78.
    call_ext_only,
    /// `bs_start_match/2` — generic opcode 79.
    bs_start_match,
    /// `bs_get_integer/5` — generic opcode 80.
    bs_get_integer,
    /// `bs_get_float/5` — generic opcode 81.
    bs_get_float,
    /// `bs_get_binary/5` — generic opcode 82.
    bs_get_binary,
    /// `bs_skip_bits/4` — generic opcode 83.
    bs_skip_bits,
    /// `bs_test_tail/2` — generic opcode 84.
    bs_test_tail,
    /// `bs_save/1` — generic opcode 85.
    bs_save,
    /// `bs_restore/1` — generic opcode 86.
    bs_restore,
    /// `bs_init/2` — generic opcode 87.
    bs_init,
    /// `bs_final/2` — generic opcode 88.
    bs_final,
    /// `bs_put_integer/5` — generic opcode 89.
    bs_put_integer,
    /// `bs_put_binary/5` — generic opcode 90.
    bs_put_binary,
    /// `bs_put_float/5` — generic opcode 91.
    bs_put_float,
    /// `bs_put_string/2` — generic opcode 92.
    bs_put_string,
    /// `bs_need_buf/1` — generic opcode 93.
    bs_need_buf,
    /// `fclearerror/0` — generic opcode 94.
    fclearerror,
    /// `fcheckerror/1` — generic opcode 95.
    fcheckerror,
    /// `fmove/2` — generic opcode 96.
    fmove,
    /// `fconv/2` — generic opcode 97.
    fconv,
    /// `fadd/4` — generic opcode 98.
    fadd,
    /// `fsub/4` — generic opcode 99.
    fsub,
    /// `fmul/4` — generic opcode 100.
    fmul,
    /// `fdiv/4` — generic opcode 101.
    fdiv,
    /// `fnegate/3` — generic opcode 102.
    fnegate,
    /// `make_fun2/1` — generic opcode 103.
    make_fun2,
    /// `try/2` — generic opcode 104.
    r#try,
    /// `try_end/1` — generic opcode 105.
    try_end,
    /// `try_case/1` — generic opcode 106.
    try_case,
    /// `try_case_end/1` — generic opcode 107.
    try_case_end,
    /// `raise/2` — generic opcode 108.
    raise,
    /// `bs_init2/6` — generic opcode 109.
    bs_init2,
    /// `bs_bits_to_bytes/3` — generic opcode 110.
    bs_bits_to_bytes,
    /// `bs_add/5` — generic opcode 111.
    bs_add,
    /// `apply/1` — generic opcode 112.
    apply,
    /// `apply_last/2` — generic opcode 113.
    apply_last,
    /// `is_boolean/2` — generic opcode 114.
    is_boolean,
    /// `is_function2/3` — generic opcode 115.
    is_function2,
    /// `bs_start_match2/5` — generic opcode 116.
    bs_start_match2,
    /// `bs_get_integer2/7` — generic opcode 117.
    bs_get_integer2,
    /// `bs_get_float2/7` — generic opcode 118.
    bs_get_float2,
    /// `bs_get_binary2/7` — generic opcode 119.
    bs_get_binary2,
    /// `bs_skip_bits2/5` — generic opcode 120.
    bs_skip_bits2,
    /// `bs_test_tail2/3` — generic opcode 121.
    bs_test_tail2,
    /// `bs_save2/2` — generic opcode 122.
    bs_save2,
    /// `bs_restore2/2` — generic opcode 123.
    bs_restore2,
    /// `gc_bif1/5` — generic opcode 124.
    gc_bif1,
    /// `gc_bif2/6` — generic opcode 125.
    gc_bif2,
    /// `bs_final2/2` — generic opcode 126.
    bs_final2,
    /// `bs_bits_to_bytes2/2` — generic opcode 127.
    bs_bits_to_bytes2,
    /// `put_literal/2` — generic opcode 128.
    put_literal,
    /// `is_bitstr/2` — generic opcode 129.
    is_bitstr,
    /// `bs_context_to_binary/1` — generic opcode 130.
    bs_context_to_binary,
    /// `bs_test_unit/3` — generic opcode 131.
    bs_test_unit,
    /// `bs_match_string/4` — generic opcode 132.
    bs_match_string,
    /// `bs_init_writable/0` — generic opcode 133.
    bs_init_writable,
    /// `bs_append/8` — generic opcode 134.
    bs_append,
    /// `bs_private_append/6` — generic opcode 135.
    bs_private_append,
    /// `trim/2` — generic opcode 136.
    trim,
    /// `bs_init_bits/6` — generic opcode 137.
    bs_init_bits,
    /// `bs_get_utf8/5` — generic opcode 138.
    bs_get_utf8,
    /// `bs_skip_utf8/4` — generic opcode 139.
    bs_skip_utf8,
    /// `bs_get_utf16/5` — generic opcode 140.
    bs_get_utf16,
    /// `bs_skip_utf16/4` — generic opcode 141.
    bs_skip_utf16,
    /// `bs_get_utf32/5` — generic opcode 142.
    bs_get_utf32,
    /// `bs_skip_utf32/4` — generic opcode 143.
    bs_skip_utf32,
    /// `bs_utf8_size/3` — generic opcode 144.
    bs_utf8_size,
    /// `bs_put_utf8/3` — generic opcode 145.
    bs_put_utf8,
    /// `bs_utf16_size/3` — generic opcode 146.
    bs_utf16_size,
    /// `bs_put_utf16/3` — generic opcode 147.
    bs_put_utf16,
    /// `bs_put_utf32/3` — generic opcode 148.
    bs_put_utf32,
    /// `on_load/0` — generic opcode 149.
    on_load,
    /// `recv_mark/1` — generic opcode 150.
    recv_mark,
    /// `recv_set/1` — generic opcode 151.
    recv_set,
    /// `gc_bif3/7` — generic opcode 152.
    gc_bif3,
    /// `line/1` — generic opcode 153.
    line,
    /// `put_map_assoc/5` — generic opcode 154.
    put_map_assoc,
    /// `put_map_exact/5` — generic opcode 155.
    put_map_exact,
    /// `is_map/2` — generic opcode 156.
    is_map,
    /// `has_map_fields/3` — generic opcode 157.
    has_map_fields,
    /// `get_map_elements/3` — generic opcode 158.
    get_map_elements,
    /// `is_tagged_tuple/4` — generic opcode 159.
    is_tagged_tuple,
    /// `build_stacktrace/0` — generic opcode 160.
    build_stacktrace,
    /// `raw_raise/0` — generic opcode 161.
    raw_raise,
    /// `get_hd/2` — generic opcode 162.
    get_hd,
    /// `get_tl/2` — generic opcode 163.
    get_tl,
    /// `put_tuple2/2` — generic opcode 164.
    put_tuple2,
    /// `bs_get_tail/3` — generic opcode 165.
    bs_get_tail,
    /// `bs_start_match3/4` — generic opcode 166.
    bs_start_match3,
    /// `bs_get_position/3` — generic opcode 167.
    bs_get_position,
    /// `bs_set_position/2` — generic opcode 168.
    bs_set_position,
    /// `swap/2` — generic opcode 169.
    swap,
    /// `bs_start_match4/4` — generic opcode 170.
    bs_start_match4,
    /// `make_fun3/3` — generic opcode 171.
    make_fun3,
    /// `init_yregs/1` — generic opcode 172.
    init_yregs,
    /// `recv_marker_bind/2` — generic opcode 173.
    recv_marker_bind,
    /// `recv_marker_clear/1` — generic opcode 174.
    recv_marker_clear,
    /// `recv_marker_reserve/1` — generic opcode 175.
    recv_marker_reserve,
    /// `recv_marker_use/1` — generic opcode 176.
    recv_marker_use,
    /// `bs_create_bin/6` — generic opcode 177.
    bs_create_bin,
    /// `call_fun2/3` — generic opcode 178.
    call_fun2,
    /// `nif_start/0` — generic opcode 179.
    nif_start,
    /// `badrecord/1` — generic opcode 180.
    badrecord,
    /// `update_record/5` — generic opcode 181.
    update_record,
    /// `bs_match/3` — generic opcode 182.
    bs_match,
    /// `executable_line/2` — generic opcode 183.
    executable_line,
    /// `debug_line/4` — generic opcode 184.
    debug_line,
    /// `bif3/6` — generic opcode 185.
    bif3,
    /// `is_any_native_record/2` — generic opcode 186.
    is_any_native_record,
    /// `is_native_record/4` — generic opcode 187.
    is_native_record,
    /// `get_record_elements/3` — generic opcode 188.
    get_record_elements,
    /// `put_record/6` — generic opcode 189.
    put_record,
    /// `is_record_accessible/3` — generic opcode 190.
    is_record_accessible,
    /// `get_record_field/5` — generic opcode 191.
    get_record_field,
    /// `get_record_elements_id/4` — generic opcode 192.
    get_record_elements_id,
    /// `update_record_id/7` — generic opcode 193.
    update_record_id,
}

/// The complete `(number, opcode, name, arity)` roster, in opcode-number order.
///
/// Covers **every** [`Opcode`] variant exactly once; `from_u8`, `as_u8`, `name` and `arity`
/// are all consistent with it. Exposed so tooling (disassemblers, differential tests) can
/// iterate the roster without a hand-maintained second list.
pub const OPCODE_TABLE: &[(u8, Opcode, &str, u8)] = &[
    (1, Opcode::label, "label", 1), (2, Opcode::func_info, "func_info", 3),
    (3, Opcode::int_code_end, "int_code_end", 0), (4, Opcode::call, "call", 2),
    (5, Opcode::call_last, "call_last", 3), (6, Opcode::call_only, "call_only", 2),
    (7, Opcode::call_ext, "call_ext", 2), (8, Opcode::call_ext_last, "call_ext_last", 3),
    (9, Opcode::bif0, "bif0", 2), (10, Opcode::bif1, "bif1", 4),
    (11, Opcode::bif2, "bif2", 5), (12, Opcode::allocate, "allocate", 2),
    (13, Opcode::allocate_heap, "allocate_heap", 3), (14, Opcode::allocate_zero, "allocate_zero", 2),
    (15, Opcode::allocate_heap_zero, "allocate_heap_zero", 3), (16, Opcode::test_heap, "test_heap", 2),
    (17, Opcode::init, "init", 1), (18, Opcode::deallocate, "deallocate", 1),
    (19, Opcode::r#return, "return", 0), (20, Opcode::send, "send", 0),
    (21, Opcode::remove_message, "remove_message", 0), (22, Opcode::timeout, "timeout", 0),
    (23, Opcode::loop_rec, "loop_rec", 2), (24, Opcode::loop_rec_end, "loop_rec_end", 1),
    (25, Opcode::wait, "wait", 1), (26, Opcode::wait_timeout, "wait_timeout", 2),
    (27, Opcode::m_plus, "m_plus", 4), (28, Opcode::m_minus, "m_minus", 4),
    (29, Opcode::m_times, "m_times", 4), (30, Opcode::m_div, "m_div", 4),
    (31, Opcode::int_div, "int_div", 4), (32, Opcode::int_rem, "int_rem", 4),
    (33, Opcode::int_band, "int_band", 4), (34, Opcode::int_bor, "int_bor", 4),
    (35, Opcode::int_bxor, "int_bxor", 4), (36, Opcode::int_bsl, "int_bsl", 4),
    (37, Opcode::int_bsr, "int_bsr", 4), (38, Opcode::int_bnot, "int_bnot", 3),
    (39, Opcode::is_lt, "is_lt", 3), (40, Opcode::is_ge, "is_ge", 3),
    (41, Opcode::is_eq, "is_eq", 3), (42, Opcode::is_ne, "is_ne", 3),
    (43, Opcode::is_eq_exact, "is_eq_exact", 3), (44, Opcode::is_ne_exact, "is_ne_exact", 3),
    (45, Opcode::is_integer, "is_integer", 2), (46, Opcode::is_float, "is_float", 2),
    (47, Opcode::is_number, "is_number", 2), (48, Opcode::is_atom, "is_atom", 2),
    (49, Opcode::is_pid, "is_pid", 2), (50, Opcode::is_reference, "is_reference", 2),
    (51, Opcode::is_port, "is_port", 2), (52, Opcode::is_nil, "is_nil", 2),
    (53, Opcode::is_binary, "is_binary", 2), (54, Opcode::is_constant, "is_constant", 2),
    (55, Opcode::is_list, "is_list", 2), (56, Opcode::is_nonempty_list, "is_nonempty_list", 2),
    (57, Opcode::is_tuple, "is_tuple", 2), (58, Opcode::test_arity, "test_arity", 3),
    (59, Opcode::select_val, "select_val", 3), (60, Opcode::select_tuple_arity, "select_tuple_arity", 3),
    (61, Opcode::jump, "jump", 1), (62, Opcode::catch, "catch", 2),
    (63, Opcode::catch_end, "catch_end", 1), (64, Opcode::r#move, "move", 2),
    (65, Opcode::get_list, "get_list", 3), (66, Opcode::get_tuple_element, "get_tuple_element", 3),
    (67, Opcode::set_tuple_element, "set_tuple_element", 3), (68, Opcode::put_string, "put_string", 3),
    (69, Opcode::put_list, "put_list", 3), (70, Opcode::put_tuple, "put_tuple", 2),
    (71, Opcode::put, "put", 1), (72, Opcode::badmatch, "badmatch", 1),
    (73, Opcode::if_end, "if_end", 0), (74, Opcode::case_end, "case_end", 1),
    (75, Opcode::call_fun, "call_fun", 1), (76, Opcode::make_fun, "make_fun", 3),
    (77, Opcode::is_function, "is_function", 2), (78, Opcode::call_ext_only, "call_ext_only", 2),
    (79, Opcode::bs_start_match, "bs_start_match", 2), (80, Opcode::bs_get_integer, "bs_get_integer", 5),
    (81, Opcode::bs_get_float, "bs_get_float", 5), (82, Opcode::bs_get_binary, "bs_get_binary", 5),
    (83, Opcode::bs_skip_bits, "bs_skip_bits", 4), (84, Opcode::bs_test_tail, "bs_test_tail", 2),
    (85, Opcode::bs_save, "bs_save", 1), (86, Opcode::bs_restore, "bs_restore", 1),
    (87, Opcode::bs_init, "bs_init", 2), (88, Opcode::bs_final, "bs_final", 2),
    (89, Opcode::bs_put_integer, "bs_put_integer", 5), (90, Opcode::bs_put_binary, "bs_put_binary", 5),
    (91, Opcode::bs_put_float, "bs_put_float", 5), (92, Opcode::bs_put_string, "bs_put_string", 2),
    (93, Opcode::bs_need_buf, "bs_need_buf", 1), (94, Opcode::fclearerror, "fclearerror", 0),
    (95, Opcode::fcheckerror, "fcheckerror", 1), (96, Opcode::fmove, "fmove", 2),
    (97, Opcode::fconv, "fconv", 2), (98, Opcode::fadd, "fadd", 4),
    (99, Opcode::fsub, "fsub", 4), (100, Opcode::fmul, "fmul", 4),
    (101, Opcode::fdiv, "fdiv", 4), (102, Opcode::fnegate, "fnegate", 3),
    (103, Opcode::make_fun2, "make_fun2", 1), (104, Opcode::r#try, "try", 2),
    (105, Opcode::try_end, "try_end", 1), (106, Opcode::try_case, "try_case", 1),
    (107, Opcode::try_case_end, "try_case_end", 1), (108, Opcode::raise, "raise", 2),
    (109, Opcode::bs_init2, "bs_init2", 6), (110, Opcode::bs_bits_to_bytes, "bs_bits_to_bytes", 3),
    (111, Opcode::bs_add, "bs_add", 5), (112, Opcode::apply, "apply", 1),
    (113, Opcode::apply_last, "apply_last", 2), (114, Opcode::is_boolean, "is_boolean", 2),
    (115, Opcode::is_function2, "is_function2", 3), (116, Opcode::bs_start_match2, "bs_start_match2", 5),
    (117, Opcode::bs_get_integer2, "bs_get_integer2", 7), (118, Opcode::bs_get_float2, "bs_get_float2", 7),
    (119, Opcode::bs_get_binary2, "bs_get_binary2", 7), (120, Opcode::bs_skip_bits2, "bs_skip_bits2", 5),
    (121, Opcode::bs_test_tail2, "bs_test_tail2", 3), (122, Opcode::bs_save2, "bs_save2", 2),
    (123, Opcode::bs_restore2, "bs_restore2", 2), (124, Opcode::gc_bif1, "gc_bif1", 5),
    (125, Opcode::gc_bif2, "gc_bif2", 6), (126, Opcode::bs_final2, "bs_final2", 2),
    (127, Opcode::bs_bits_to_bytes2, "bs_bits_to_bytes2", 2), (128, Opcode::put_literal, "put_literal", 2),
    (129, Opcode::is_bitstr, "is_bitstr", 2), (130, Opcode::bs_context_to_binary, "bs_context_to_binary", 1),
    (131, Opcode::bs_test_unit, "bs_test_unit", 3), (132, Opcode::bs_match_string, "bs_match_string", 4),
    (133, Opcode::bs_init_writable, "bs_init_writable", 0), (134, Opcode::bs_append, "bs_append", 8),
    (135, Opcode::bs_private_append, "bs_private_append", 6), (136, Opcode::trim, "trim", 2),
    (137, Opcode::bs_init_bits, "bs_init_bits", 6), (138, Opcode::bs_get_utf8, "bs_get_utf8", 5),
    (139, Opcode::bs_skip_utf8, "bs_skip_utf8", 4), (140, Opcode::bs_get_utf16, "bs_get_utf16", 5),
    (141, Opcode::bs_skip_utf16, "bs_skip_utf16", 4), (142, Opcode::bs_get_utf32, "bs_get_utf32", 5),
    (143, Opcode::bs_skip_utf32, "bs_skip_utf32", 4), (144, Opcode::bs_utf8_size, "bs_utf8_size", 3),
    (145, Opcode::bs_put_utf8, "bs_put_utf8", 3), (146, Opcode::bs_utf16_size, "bs_utf16_size", 3),
    (147, Opcode::bs_put_utf16, "bs_put_utf16", 3), (148, Opcode::bs_put_utf32, "bs_put_utf32", 3),
    (149, Opcode::on_load, "on_load", 0), (150, Opcode::recv_mark, "recv_mark", 1),
    (151, Opcode::recv_set, "recv_set", 1), (152, Opcode::gc_bif3, "gc_bif3", 7),
    (153, Opcode::line, "line", 1), (154, Opcode::put_map_assoc, "put_map_assoc", 5),
    (155, Opcode::put_map_exact, "put_map_exact", 5), (156, Opcode::is_map, "is_map", 2),
    (157, Opcode::has_map_fields, "has_map_fields", 3), (158, Opcode::get_map_elements, "get_map_elements", 3),
    (159, Opcode::is_tagged_tuple, "is_tagged_tuple", 4), (160, Opcode::build_stacktrace, "build_stacktrace", 0),
    (161, Opcode::raw_raise, "raw_raise", 0), (162, Opcode::get_hd, "get_hd", 2),
    (163, Opcode::get_tl, "get_tl", 2), (164, Opcode::put_tuple2, "put_tuple2", 2),
    (165, Opcode::bs_get_tail, "bs_get_tail", 3), (166, Opcode::bs_start_match3, "bs_start_match3", 4),
    (167, Opcode::bs_get_position, "bs_get_position", 3), (168, Opcode::bs_set_position, "bs_set_position", 2),
    (169, Opcode::swap, "swap", 2), (170, Opcode::bs_start_match4, "bs_start_match4", 4),
    (171, Opcode::make_fun3, "make_fun3", 3), (172, Opcode::init_yregs, "init_yregs", 1),
    (173, Opcode::recv_marker_bind, "recv_marker_bind", 2), (174, Opcode::recv_marker_clear, "recv_marker_clear", 1),
    (175, Opcode::recv_marker_reserve, "recv_marker_reserve", 1), (176, Opcode::recv_marker_use, "recv_marker_use", 1),
    (177, Opcode::bs_create_bin, "bs_create_bin", 6), (178, Opcode::call_fun2, "call_fun2", 3),
    (179, Opcode::nif_start, "nif_start", 0), (180, Opcode::badrecord, "badrecord", 1),
    (181, Opcode::update_record, "update_record", 5), (182, Opcode::bs_match, "bs_match", 3),
    (183, Opcode::executable_line, "executable_line", 2), (184, Opcode::debug_line, "debug_line", 4),
    (185, Opcode::bif3, "bif3", 6), (186, Opcode::is_any_native_record, "is_any_native_record", 2),
    (187, Opcode::is_native_record, "is_native_record", 4), (188, Opcode::get_record_elements, "get_record_elements", 3),
    (189, Opcode::put_record, "put_record", 6), (190, Opcode::is_record_accessible, "is_record_accessible", 3),
    (191, Opcode::get_record_field, "get_record_field", 5), (192, Opcode::get_record_elements_id, "get_record_elements_id", 4),
    (193, Opcode::update_record_id, "update_record_id", 7),
];

/// The highest generic opcode this runtime understands.
///
/// The last numbered entry in `../../lib/compiler/src/genop.tab`
/// (`193: update_record_id/7`). The C derives the same value from the very same roster
/// (`erts/emulator/utils/beam_makeops:1005` emits `#define MAX_GENERIC_OPCODE
/// $num_file_opcodes-1`) and uses it as *both* the opcode-byte bound
/// (`beam_file.c:2295`) and the `Code`-header upper bound (`beam_load.c:189-196`).
/// `opcode_roster_bounds_match_the_table` pins this constant to [`OPCODE_TABLE`].
pub const MAX_GENERIC_OPCODE: u8 = 193;

/// The lowest `Code`-header `max_opcode` a loadable image may declare.
///
/// `swap/2` ([`Opcode::swap`]) — generic opcode 169 (`genop.tab:611`), introduced in OTP 23.
/// OTP 23-26 set the header's `max_opcode` to exactly this opcode, so a *smaller* value
/// means the file was produced by OTP 22 or earlier, which the current runtime refuses
/// (`beam_load.c:197-225`). `opcode_roster_bounds_match_the_table` pins it to
/// `Opcode::swap.as_u8()`.
pub const MIN_SUPPORTED_MAX_OPCODE: u8 = 169;

impl Opcode {
    /// Maps a wire opcode byte to its [`Opcode`], or `None` if the byte is not a generic
    /// opcode (i.e. outside `1..=193`).
    pub fn from_u8(b: u8) -> Option<Opcode> {
        Some(match b {
            1 => Opcode::label,
            2 => Opcode::func_info,
            3 => Opcode::int_code_end,
            4 => Opcode::call,
            5 => Opcode::call_last,
            6 => Opcode::call_only,
            7 => Opcode::call_ext,
            8 => Opcode::call_ext_last,
            9 => Opcode::bif0,
            10 => Opcode::bif1,
            11 => Opcode::bif2,
            12 => Opcode::allocate,
            13 => Opcode::allocate_heap,
            14 => Opcode::allocate_zero,
            15 => Opcode::allocate_heap_zero,
            16 => Opcode::test_heap,
            17 => Opcode::init,
            18 => Opcode::deallocate,
            19 => Opcode::r#return,
            20 => Opcode::send,
            21 => Opcode::remove_message,
            22 => Opcode::timeout,
            23 => Opcode::loop_rec,
            24 => Opcode::loop_rec_end,
            25 => Opcode::wait,
            26 => Opcode::wait_timeout,
            27 => Opcode::m_plus,
            28 => Opcode::m_minus,
            29 => Opcode::m_times,
            30 => Opcode::m_div,
            31 => Opcode::int_div,
            32 => Opcode::int_rem,
            33 => Opcode::int_band,
            34 => Opcode::int_bor,
            35 => Opcode::int_bxor,
            36 => Opcode::int_bsl,
            37 => Opcode::int_bsr,
            38 => Opcode::int_bnot,
            39 => Opcode::is_lt,
            40 => Opcode::is_ge,
            41 => Opcode::is_eq,
            42 => Opcode::is_ne,
            43 => Opcode::is_eq_exact,
            44 => Opcode::is_ne_exact,
            45 => Opcode::is_integer,
            46 => Opcode::is_float,
            47 => Opcode::is_number,
            48 => Opcode::is_atom,
            49 => Opcode::is_pid,
            50 => Opcode::is_reference,
            51 => Opcode::is_port,
            52 => Opcode::is_nil,
            53 => Opcode::is_binary,
            54 => Opcode::is_constant,
            55 => Opcode::is_list,
            56 => Opcode::is_nonempty_list,
            57 => Opcode::is_tuple,
            58 => Opcode::test_arity,
            59 => Opcode::select_val,
            60 => Opcode::select_tuple_arity,
            61 => Opcode::jump,
            62 => Opcode::catch,
            63 => Opcode::catch_end,
            64 => Opcode::r#move,
            65 => Opcode::get_list,
            66 => Opcode::get_tuple_element,
            67 => Opcode::set_tuple_element,
            68 => Opcode::put_string,
            69 => Opcode::put_list,
            70 => Opcode::put_tuple,
            71 => Opcode::put,
            72 => Opcode::badmatch,
            73 => Opcode::if_end,
            74 => Opcode::case_end,
            75 => Opcode::call_fun,
            76 => Opcode::make_fun,
            77 => Opcode::is_function,
            78 => Opcode::call_ext_only,
            79 => Opcode::bs_start_match,
            80 => Opcode::bs_get_integer,
            81 => Opcode::bs_get_float,
            82 => Opcode::bs_get_binary,
            83 => Opcode::bs_skip_bits,
            84 => Opcode::bs_test_tail,
            85 => Opcode::bs_save,
            86 => Opcode::bs_restore,
            87 => Opcode::bs_init,
            88 => Opcode::bs_final,
            89 => Opcode::bs_put_integer,
            90 => Opcode::bs_put_binary,
            91 => Opcode::bs_put_float,
            92 => Opcode::bs_put_string,
            93 => Opcode::bs_need_buf,
            94 => Opcode::fclearerror,
            95 => Opcode::fcheckerror,
            96 => Opcode::fmove,
            97 => Opcode::fconv,
            98 => Opcode::fadd,
            99 => Opcode::fsub,
            100 => Opcode::fmul,
            101 => Opcode::fdiv,
            102 => Opcode::fnegate,
            103 => Opcode::make_fun2,
            104 => Opcode::r#try,
            105 => Opcode::try_end,
            106 => Opcode::try_case,
            107 => Opcode::try_case_end,
            108 => Opcode::raise,
            109 => Opcode::bs_init2,
            110 => Opcode::bs_bits_to_bytes,
            111 => Opcode::bs_add,
            112 => Opcode::apply,
            113 => Opcode::apply_last,
            114 => Opcode::is_boolean,
            115 => Opcode::is_function2,
            116 => Opcode::bs_start_match2,
            117 => Opcode::bs_get_integer2,
            118 => Opcode::bs_get_float2,
            119 => Opcode::bs_get_binary2,
            120 => Opcode::bs_skip_bits2,
            121 => Opcode::bs_test_tail2,
            122 => Opcode::bs_save2,
            123 => Opcode::bs_restore2,
            124 => Opcode::gc_bif1,
            125 => Opcode::gc_bif2,
            126 => Opcode::bs_final2,
            127 => Opcode::bs_bits_to_bytes2,
            128 => Opcode::put_literal,
            129 => Opcode::is_bitstr,
            130 => Opcode::bs_context_to_binary,
            131 => Opcode::bs_test_unit,
            132 => Opcode::bs_match_string,
            133 => Opcode::bs_init_writable,
            134 => Opcode::bs_append,
            135 => Opcode::bs_private_append,
            136 => Opcode::trim,
            137 => Opcode::bs_init_bits,
            138 => Opcode::bs_get_utf8,
            139 => Opcode::bs_skip_utf8,
            140 => Opcode::bs_get_utf16,
            141 => Opcode::bs_skip_utf16,
            142 => Opcode::bs_get_utf32,
            143 => Opcode::bs_skip_utf32,
            144 => Opcode::bs_utf8_size,
            145 => Opcode::bs_put_utf8,
            146 => Opcode::bs_utf16_size,
            147 => Opcode::bs_put_utf16,
            148 => Opcode::bs_put_utf32,
            149 => Opcode::on_load,
            150 => Opcode::recv_mark,
            151 => Opcode::recv_set,
            152 => Opcode::gc_bif3,
            153 => Opcode::line,
            154 => Opcode::put_map_assoc,
            155 => Opcode::put_map_exact,
            156 => Opcode::is_map,
            157 => Opcode::has_map_fields,
            158 => Opcode::get_map_elements,
            159 => Opcode::is_tagged_tuple,
            160 => Opcode::build_stacktrace,
            161 => Opcode::raw_raise,
            162 => Opcode::get_hd,
            163 => Opcode::get_tl,
            164 => Opcode::put_tuple2,
            165 => Opcode::bs_get_tail,
            166 => Opcode::bs_start_match3,
            167 => Opcode::bs_get_position,
            168 => Opcode::bs_set_position,
            169 => Opcode::swap,
            170 => Opcode::bs_start_match4,
            171 => Opcode::make_fun3,
            172 => Opcode::init_yregs,
            173 => Opcode::recv_marker_bind,
            174 => Opcode::recv_marker_clear,
            175 => Opcode::recv_marker_reserve,
            176 => Opcode::recv_marker_use,
            177 => Opcode::bs_create_bin,
            178 => Opcode::call_fun2,
            179 => Opcode::nif_start,
            180 => Opcode::badrecord,
            181 => Opcode::update_record,
            182 => Opcode::bs_match,
            183 => Opcode::executable_line,
            184 => Opcode::debug_line,
            185 => Opcode::bif3,
            186 => Opcode::is_any_native_record,
            187 => Opcode::is_native_record,
            188 => Opcode::get_record_elements,
            189 => Opcode::put_record,
            190 => Opcode::is_record_accessible,
            191 => Opcode::get_record_field,
            192 => Opcode::get_record_elements_id,
            193 => Opcode::update_record_id,
            _ => return None,
        })
    }

    /// The wire opcode byte for this opcode (the inverse of [`Opcode::from_u8`]).
    pub fn as_u8(self) -> u8 {
        self.entry().0
    }

    /// The op name exactly as spelled in `genop.tab` (e.g. `"gc_bif2"`, `"call_ext_only"`).
    pub fn name(self) -> &'static str {
        self.entry().2
    }

    /// The number of operands this opcode carries in the instruction stream.
    pub fn arity(self) -> u8 {
        self.entry().3
    }

    /// The [`OPCODE_TABLE`] row for this opcode.
    ///
    /// Infallible: the table is generated from the same roster as the enum, so it contains
    /// exactly one row per variant. The `expect` guards a compile-time invariant (generated
    /// data), never runtime input.
    fn entry(self) -> &'static (u8, Opcode, &'static str, u8) {
        OPCODE_TABLE
            .iter()
            .find(|entry| entry.1 == self)
            .expect("OPCODE_TABLE covers every Opcode variant")
    }
}

#[cfg(test)]
mod tests {
    //! The two roster-bound constants [`MAX_GENERIC_OPCODE`] / [`MIN_SUPPORTED_MAX_OPCODE`]
    //! are derived from the roster, never guessed: these tests pin them to [`OPCODE_TABLE`]
    //! so a `genop.tab` regeneration that moves either boundary fails here.

    use super::*;

    #[test]
    fn opcode_roster_bounds_match_the_table() {
        let highest = OPCODE_TABLE.iter().map(|entry| entry.0).max().unwrap();
        assert_eq!(
            MAX_GENERIC_OPCODE, highest,
            "MAX_GENERIC_OPCODE must be the last numbered genop.tab opcode"
        );
        assert_eq!(
            MIN_SUPPORTED_MAX_OPCODE,
            Opcode::swap.as_u8(),
            "the lower header bound is the `swap/2` opcode"
        );
    }

    #[test]
    fn every_numbered_table_row_is_contiguous_from_one() {
        // `Opcode::from_u8` maps `1..=MAX_GENERIC_OPCODE` exhaustively (a byte above the
        // roster is rejected by `decode_stream`, and `from_u8(0)` is not an opcode).
        for byte in 1..=MAX_GENERIC_OPCODE {
            let opcode = Opcode::from_u8(byte)
                .unwrap_or_else(|| panic!("byte {byte} has no roster entry"));
            assert_eq!(opcode.as_u8(), byte, "roster round-trip for byte {byte}");
        }
        assert!(Opcode::from_u8(0).is_none());
        assert!(Opcode::from_u8(MAX_GENERIC_OPCODE + 1).is_none());
    }
}
