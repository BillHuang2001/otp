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
//! Contracts **A.7** / **A.8** — the `erlang` BIF family beyond the slices the sibling groups
//! already own: the `process_info/1,2` item set, the **local-fun** identity surface
//! (`erlang:fun_info/1,2` of a fun a compiled module created) and the remaining cheap
//! `erlang:*` shapes the divergence ledger still names.
//!
//! Every case here **agrees**: the Rust runtime reproduces the oracle's observable result
//! exactly. Nothing in this group is a recorded capability gap, so nothing here relies on
//! `known-gaps.txt`.
//!
//! # The two `%T` renderers (A.7)
//!
//! `erlang:display/1` and `erts_internal:term_to_string/1,2` are both the C's `%T` printer
//! (`erl_printf_term.c`): the first writes the rendering plus the `\r\n` its `write_f_add_cr`
//! appends, the second answers a **character list** and takes a byte **budget** whose overrun
//! appends the `"... "` truncation marker.
//!
//! A `display/1` call *before* the probe's own line is pinnable byte-exactly, because a
//! [`Golden`]'s stdout is the **whole** probe stdout minus the final newline
//! (`crate::compare::normalize_probe_stdout`): the golden therefore reads
//! `<rendered text>\r\nok <value>`. That is what pins the `%T` quoting rules (a printable list
//! becomes a quoted string, a byte list of a binary either the quoted form or the numbered
//! form, an atom that needs quotes gets them, a float uses the `%e` shape) and the CRLF
//! terminator itself.
//!
//! `term_to_string/1,2` is pinned as the `~w` image of the returned character list — the
//! exact bytes — or, where several renderings are compared at once, as an `=:=` matrix whose
//! expected `%T` strings live in the expression. A golden of `ok [true,…]` still pins them:
//! the *oracle* must reproduce it (`check_golden`), so a wrong rendering is corpus drift.
//!
//! # Why five `process_info` items are absent from this group
//!
//! `parent`, `label`, `priority_messages`, `magic_ref`, `garbage_collection_info` (and
//! `backtrace`) are **typed capability gaps** in the runtime — `UNSUPPORTED_ITEMS` in
//! `crates/beam_rt/src/native/memory/mod.rs`, where each names the missing accessor — so the
//! query exits `3` with a `beam_rt: unsupported: process_info: <item> (…)` diagnostic. A case
//! over one of them would classify as `unsupported`, never as an agreement, and this group
//! asks for agreements only. `last_calls` *is* answered (always `false`), which is why it is
//! pinnable **only** in its no-buffer state: a process with an *armed* `save_calls` buffer
//! (`[]` when nothing has been recorded yet, a list of `{M,F,A}`/`send`/`receive` entries
//! otherwise) is a runtime-side gap too. Both are reported to the runtime, never recorded
//! here and never allowlisted.
//!
//! # Why the module-run route (and not only `-eval`)
//!
//! Two of the surfaces this group exists for cannot be reached from an `-eval` expression:
//!
//! * a **local** fun literal cannot be built by the oracle's `erl_eval` (`fun(X) -> X + 1 end`
//!   evaluated by `-eval` produces an *`erl_eval`* fun, whose `fun_info/1` is a different
//!   animal — and on the Rust side an `erl_eval` closure has no faithful identity image at all,
//!   so it stays a typed `unsupported`), and
//! * the `process_info/2` **execution-position** items (`current_function`, `initial_call`,
//!   `current_location`, `current_stacktrace`, `catchlevel`) are only readable for a process the
//!   runtime actually scheduled an engine for — a fun-bodied process spawned from `-eval` has no
//!   resolved target on the Rust side, so those items are a typed `unsupported` there.
//!
//! Both become ordinary agreement facts once the module is compiled by `erlc` and run as real
//! bytecode: a compiled local fun has a real `FunT` entry (module/name/index/uniq/…), and a
//! process spawned from compiled code parks in a frame the runtime can name. So the bulk of this
//! group is [`CaseKind::Module`] over one embedded module, [`SOURCE`].
//!
//! # The module-run golden is an integer — the *number of agreeing pairs*
//!
//! The corpus-wide module-run rule (see `super::tests` in `corpus/mod.rs`) is that a module-run
//! golden is a plain integer line, so every function of [`SOURCE`] returns
//! `agree(Observed, Expected)`: the length of the observed list when it equals `Expected`
//! element for element, minus the disagreeing positions — i.e. a golden of `N` asserts *all
//! `N`* of the pairs the function checks, and a diff like `18` against `20` names how many went
//! wrong. `Expected` lives in the module source, so the observation is pinned exactly while the
//! golden stays an integer.
//!
//! # What the expectations deliberately do NOT pin
//!
//! * A **per-VM-start** value: a pid, a reference, a port id, a creation. The fun's `pid` item
//!   (the creating process) is reduced to `is_pid/1`, the `monitored_by`/`monitors` items to a
//!   count and an origin atom, the `group_leader` item to its *kind*.
//! * A **compiler-version-derived** hash. `fun_info(F, uniq)` (an integer), `fun_info(F,
//!   new_uniq)` (a 16-byte binary) and `module_info(attributes)`'s `vsn` are md5s of the compiled
//!   code, so both runtimes agree within a run (they compile the same source with the same
//!   `erlc`) but the *literal* is not portable across compiler releases. They are pinned as
//!   **shapes** (`is_integer/1`, `byte_size/1`, `hd/1` is an integer) instead — the same
//!   discipline the sibling `modules/fun_info_external_fun_created_in_compiled_code` case (which
//!   could only pin the *external* fun, because the local-fun surface was exit-3 at the time it
//!   landed) uses.
//! * A **per-VM-unstable rendering**: the process **dictionary** is a hash table, so the oracle
//!   emits `[{a,1},{b,2}]` on one VM start and `[{b,2},{a,1}]` on the next (measured 2/6 vs 4/6
//!   here) — the case compares `lists:sort/1` of the entries. A multi-key atom-keyed **map** is
//!   the same story, so `max_heap_size`/`garbage_collection` are pinned by *kind* (`map`,
//!   `{list,5}`) only.
//! * A **runtime-internal quantity**: `memory`, `stack_size`, `reductions`, `total_heap_size`
//!   and `group_leader`'s value differ by construction (the Rust heap is a different size and a
//!   single-threaded scheduler counts differently), so the shape case pins their *type* rather
//!   than their value, and the value case pins only the items that really are equal.
//!
//! # Known traps for whoever edits this group
//!
//! * A probe expression must not bind `V`, `C` or `R` (the probe wrapper binds them), and one
//!   containing a `case` or a `<<…>>` pattern must be wrapped in an immediately-applied fun.
//!   Every expression here is a plain `begin … end`/list-comprehension/tuple shape.
//! * `pi_location/0`'s expectation carries the **source line** of the parked child's `receive`.
//!   That line is a function of [`SOURCE`]'s line layout, so the
//!   `the_location_expectation_names_the_parked_childs_source_line` test recomputes it from
//!   `SOURCE` and fails the build if an edit shifts it without the module following.
//! * No case blocks: the parent's every `receive` is bounded by `after`, and the child's
//!   unbounded `receive stop -> ok end` is the *parked state* the case observes (nothing is
//!   waiting for it to return).

use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The module name of [`SOURCE`] (must match its `-module` attribute).
const MODULE: &str = "diff_bif_extra";

/// The embedded Erlang module every module-run case of this group executes.
///
/// Every exported function returns `agree(Observed, Expected)` — the number of pairs of the two
/// lists that are equal — so the golden is an integer and the expectation is written out in
/// Erlang (see the module docs above for what is reduced to a shape and why).
const SOURCE: &str = r#"-module(diff_bif_extra).
-export([fun_local_items/0, fun_local_index_items/0, fun_local_env/0,
         fun_captured_env/0, fun_pid_item/0, fun_whole_list/0,
         pi_position/0, pi_location/0, pi_stack/0, pi_exact/0, pi_shape/0,
         pi_keys/0, pi_registered_keys/0, pi_messages/0, pi_dict/0,
         pi_links/0, pi_monitors/0, pi_dead/0, pi_badarg/0, pi_list_order/0,
         mod_info/1]).

agree(Obs, Exp) when length(Obs) =/= length(Exp) -> -1;
agree(Obs, Exp) -> length([ok || {O, E} <- lists:zip(Obs, Exp), O =:= E]).

local_fun() -> fun(X) -> X + 1 end.
capture_fun(N) -> fun(X) -> X + N end.

fun_local_items() ->
    agree([erlang:fun_info(local_fun(), K) || K <- [module, name, arity, type]],
          [{module, diff_bif_extra}, {name, '-local_fun/0-fun-0-'},
           {arity, 1}, {type, local}]).

fun_local_index_items() ->
    agree([erlang:fun_info(local_fun(), index),
           erlang:fun_info(local_fun(), new_index),
           is_integer(element(2, erlang:fun_info(local_fun(), uniq))),
           byte_size(element(2, erlang:fun_info(local_fun(), new_uniq)))],
          [{index, 0}, {new_index, 0}, true, 16]).

fun_local_env() ->
    agree([erlang:fun_info(local_fun(), env)], [{env, []}]).

fun_captured_env() ->
    agree([erlang:fun_info(capture_fun(7), env)], [{env, [7]}]).

fun_pid_item() ->
    Item = erlang:fun_info(local_fun(), pid),
    agree([element(1, Item), is_pid(element(2, Item))], [pid, true]).

shape(pid, Val) -> {pid, is_pid(Val)};
shape(uniq, Val) when is_integer(Val) -> {uniq, integer};
shape(new_uniq, B) -> {new_uniq, {byte_size, byte_size(B)}};
shape(K, Val) -> {K, Val}.

fun_whole_list() ->
    agree(lists:sort([shape(K, Val) || {K, Val} <- erlang:fun_info(local_fun())]),
          [{arity, 1}, {env, []}, {index, 0}, {module, diff_bif_extra},
           {name, '-local_fun/0-fun-0-'}, {new_index, 0},
           {new_uniq, {byte_size, 16}}, {pid, true}, {type, local},
           {uniq, integer}]).

parked() ->
    Parent = self(),
    P = spawn(fun() -> Parent ! ready, receive stop -> ok end end),
    ready = receive ready -> ready after 2000 -> timeout end,
    P.

pi_position() ->
    P = parked(),
    agree([erlang:process_info(P, I)
           || I <- [current_function, initial_call, catchlevel, status]],
          [{current_function, {diff_bif_extra, '-parked/0-fun-0-', 1}},
           {initial_call, {erlang, apply, 2}}, {catchlevel, 0},
           {status, waiting}]).

pi_location() ->
    P = parked(),
    {current_location, {M, F, A, Loc}} = erlang:process_info(P, current_location),
    agree([M, F, A, [K || {K, _} <- Loc], proplists:get_value(line, Loc)],
          [diff_bif_extra, '-parked/0-fun-0-', 1, [file, line], 51]).

pi_stack() ->
    P = parked(),
    {current_stacktrace, St} = erlang:process_info(P, current_stacktrace),
    agree([{M, F, A, length(Loc)} || {M, F, A, Loc} <- St],
          [{diff_bif_extra, '-parked/0-fun-0-', 1, 2}]).

pi_exact() ->
    P = parked(),
    agree([erlang:process_info(P, I)
           || I <- [message_queue_len, links, monitors, monitored_by, dictionary,
                    trap_exit, error_handler, priority, total_heap_size, heap_size,
                    min_heap_size, min_bin_vheap_size, fullsweep_after,
                    message_queue_data, trace, sequential_trace_token, suspending,
                    last_calls, binary, registered_name]],
          [{message_queue_len, 0}, {links, []}, {monitors, []}, {monitored_by, []},
           {dictionary, []}, {trap_exit, false}, {error_handler, error_handler},
           {priority, normal}, {total_heap_size, 233}, {heap_size, 233},
           {min_heap_size, 233}, {min_bin_vheap_size, 46422},
           {fullsweep_after, 65535}, {message_queue_data, on_heap}, {trace, 0},
           {sequential_trace_token, []}, {suspending, []}, {last_calls, false},
           {binary, []}, []]).

kind(Val) when is_pid(Val) -> pid;
kind(Val) when is_integer(Val) -> integer;
kind(Val) when is_atom(Val) -> atom;
kind(Val) when is_map(Val) -> map;
kind(Val) when is_tuple(Val) -> tuple;
kind(Val) when is_list(Val) -> {list, length(Val)}.

pi_shape() ->
    P = parked(),
    agree([{I, kind(element(2, erlang:process_info(P, I)))}
           || I <- [status, message_queue_len, links, monitors, monitored_by,
                    dictionary, trap_exit, error_handler, priority, group_leader,
                    current_function, initial_call, current_location,
                    current_stacktrace, catchlevel, total_heap_size, heap_size,
                    stack_size, reductions, garbage_collection, min_heap_size,
                    min_bin_vheap_size, fullsweep_after, max_heap_size,
                    message_queue_data, trace, sequential_trace_token, suspending,
                    last_calls, memory, binary]],
          [{status, atom}, {message_queue_len, integer}, {links, {list, 0}},
           {monitors, {list, 0}}, {monitored_by, {list, 0}},
           {dictionary, {list, 0}}, {trap_exit, atom}, {error_handler, atom},
           {priority, atom}, {group_leader, pid}, {current_function, tuple},
           {initial_call, tuple}, {current_location, tuple},
           {current_stacktrace, {list, 1}}, {catchlevel, integer},
           {total_heap_size, integer}, {heap_size, integer},
           {stack_size, integer}, {reductions, integer},
           {garbage_collection, {list, 5}}, {min_heap_size, integer},
           {min_bin_vheap_size, integer}, {fullsweep_after, integer},
           {max_heap_size, map}, {message_queue_data, atom}, {trace, integer},
           {sequential_trace_token, {list, 0}}, {suspending, {list, 0}},
           {last_calls, atom}, {memory, integer}, {binary, {list, 0}}]).

pi_keys() ->
    P = parked(),
    agree([K || {K, _} <- erlang:process_info(P)],
          [current_function, initial_call, status, message_queue_len, links,
           dictionary, trap_exit, error_handler, priority, group_leader,
           total_heap_size, heap_size, stack_size, reductions,
           garbage_collection, suspending]).

pi_registered_keys() ->
    Parent = self(),
    P = spawn(fun() -> register(diff_reg_probe, self()), Parent ! ready,
                          receive stop -> ok end end),
    ready = receive ready -> ready after 2000 -> timeout end,
    Keys = [K || {K, _} <- erlang:process_info(P)],
    unregister(diff_reg_probe),
    agree(Keys,
          [registered_name, current_function, initial_call, status,
           message_queue_len, links, dictionary, trap_exit, error_handler,
           priority, group_leader, total_heap_size, heap_size, stack_size,
           reductions, garbage_collection, suspending]).

pi_messages() ->
    P = parked(),
    P ! one, P ! two, P ! three,
    agree([erlang:process_info(P, message_queue_len),
           erlang:process_info(P, messages)],
          [{message_queue_len, 3}, {messages, [one, two, three]}]).

pi_dict() ->
    Parent = self(),
    P = spawn(fun() -> put(a, 1), put(b, 2), Parent ! ready,
                          receive stop -> ok end end),
    ready = receive ready -> ready after 2000 -> timeout end,
    agree([lists:sort(element(2, erlang:process_info(P, dictionary))),
           erlang:process_info(P, {dictionary, a}),
           erlang:process_info(P, {dictionary, absent})],
          [[{a, 1}, {b, 2}], {{dictionary, a}, 1},
           {{dictionary, absent}, undefined}]).

pi_links() ->
    P = parked(),
    Before = element(2, erlang:process_info(P, links)),
    true = link(P),
    After = length(element(2, erlang:process_info(P, links))),
    true = unlink(P),
    Final = element(2, erlang:process_info(P, links)),
    agree([Before, After, Final], [[], 1, []]).

pi_monitors() ->
    P = parked(),
    MRef = erlang:monitor(process, P),
    Thered = length(element(2, erlang:process_info(P, monitored_by))),
    Mine = [T || {T, _} <- element(2, erlang:process_info(self(), monitors))],
    erlang:demonitor(MRef, [flush]),
    agree([Thered, Mine], [1, [process]]).

pi_dead() ->
    Parent = self(),
    P = spawn(fun() -> Parent ! dying end),
    dying = receive dying -> dying after 2000 -> timeout end,
    Ref = erlang:monitor(process, P),
    receive {'DOWN', Ref, process, P, _} -> ok after 2000 -> timeout end,
    agree([erlang:process_info(P, status), erlang:process_info(P, [status])],
          [undefined, undefined]).

cls(F) ->
    try F() of _ -> no_raise catch Cls:Reason -> {Cls, Reason} end.

pi_badarg() ->
    P = parked(),
    agree([cls(fun() -> erlang:process_info(P, bogus_item) end),
           cls(fun() -> erlang:process_info(not_a_pid, status) end),
           cls(fun() -> erlang:process_info(P, 42) end)],
          [{error, badarg}, {error, badarg}, {error, badarg}]).

pi_list_order() ->
    P = parked(),
    agree(erlang:process_info(P, [priority, priority, trap_exit]),
          [{priority, normal}, {priority, normal}, {trap_exit, false}]).

mod_info(0) ->
    agree(lists:sort(?MODULE:module_info(exports)),
          [{fun_captured_env, 0}, {fun_local_env, 0}, {fun_local_index_items, 0},
           {fun_local_items, 0}, {fun_pid_item, 0}, {fun_whole_list, 0},
           {mod_info, 1}, {module_info, 0}, {module_info, 1}, {pi_badarg, 0},
           {pi_dead, 0}, {pi_dict, 0}, {pi_exact, 0}, {pi_keys, 0},
           {pi_links, 0}, {pi_list_order, 0}, {pi_location, 0},
           {pi_messages, 0}, {pi_monitors, 0}, {pi_position, 0},
           {pi_registered_keys, 0}, {pi_shape, 0}, {pi_stack, 0}]);
mod_info(1) ->
    agree([?MODULE:module_info(module)], [diff_bif_extra]);
mod_info(2) ->
    Attrs = ?MODULE:module_info(attributes),
    agree([[K || {K, _} <- Attrs], [is_integer(hd(Val)) || {_, Val} <- Attrs]],
          [[vsn], [true]]).
"#;

/// One A.8 introspection expression case (shared `-eval` argv, `~w` rendering).
fn info_case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
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

/// One A.7 `erlang:*` BIF-surface expression case.
fn bif_case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::OtpApps],
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

/// One module-run case: apply `<MODULE>:<function>/<arity>` to the integer `args`.
///
/// A module-run case evidences the `.beam` contract by construction (the source is compiled by
/// `erlc` and executed as real bytecode), so `BeamAsm` is always one of its contracts.
fn module_case(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    function: &'static str,
    arity: u32,
    args: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Module(ModuleRun {
            source: SOURCE,
            module: MODULE,
            function,
            arity,
            args,
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// One arity-0 module-run case over [`SOURCE`].
fn module_case0(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    function: &'static str,
    golden: &'static str,
) -> Case {
    module_case(id, description, contracts, function, 0, &[], golden)
}

/// `erts_internal:term_to_string/1` over one term of each kind, compared with the `%T` text the
/// oracle renders (the expected strings are measured, not guessed: the float's `%e` shape, a
/// single-key map's `#{k=>v}` form, a printable list as a quoted string and a printable binary
/// as `<<"…">>` with escaped quotes).
const TERM_TO_STRING_KINDS: &str = r##"[erts_internal:term_to_string(T) =:= S || {T,S} <- [{1.5,"1.500000e+00"},{#{a=>1},"#{a=>1}"},{"hello","\"hello\""},{<<"xy">>,"<<\"xy\">>"}]]"##;

/// `erts_internal:term_to_string/2`'s accepted budget arms, compared with the measured
/// renderings. Note the two counter-intuitive ones the C really produces: a budget of `4` is
/// enough for `{a,1}` *and still* appends the marker (`"{a,1}... "`, nine bytes for a five-byte
/// term), while `2` truncates mid-term (`"{a... "`); `undefined` and `INT_MAX - 1` are the
/// unlimited forms.
const TERM_TO_STRING_LIMITS: &str = r#"[erts_internal:term_to_string({a,1}, B) =:= S || {B,S} <- [{4,"{a,1}... "},{2,"{a... "},{undefined,"{a,1}"},{2147483646,"{a,1}"}]]"#;

/// `erts_internal:term_to_string/2` over the limits the C's argument check rejects
/// (`bif.c:4168-4176`: `1 < L < INT_MAX` or the atom `undefined`) — including `INT_MAX` itself,
/// which is *not* read as "unlimited", and a non-integer/non-atom limit; each refusal is
/// classified into `{Class, Reason}` so the golden is order-stable.
const TERM_TO_STRING_BADARG_MATRIX: &str = "[try erts_internal:term_to_string({a,1}, L) of _ -> ok catch Cls:Rs -> {Cls, Rs} end || L <- [1, 0, -1, 2147483647, foo]]";

/// The `bif_extra` cases.
pub(crate) fn group() -> Vec<Case> {
    use Contract::{BeamAsm, OtpApps};
    vec![
        // --- the local-fun identity surface (A.8), from real bytecode ----------------------
        module_case0(
            "bif_extra/fun_info_local_identity_items",
            "fun_info/2 of a compiled local fun answers all four identity items (module, name, \
             arity, type)",
            &[BeamAsm, OtpApps],
            "fun_local_items",
            "4",
        ),
        module_case0(
            "bif_extra/fun_info_local_index_and_hash_shape_items",
            "a local fun's index and new_index are 0 (its fun-table slot) and its uniq/new_uniq \
             are the compiler's hash shapes — integer and 16 bytes — not literals",
            &[BeamAsm, OtpApps],
            "fun_local_index_items",
            "4",
        ),
        module_case0(
            "bif_extra/fun_info_local_env_is_empty",
            "fun_info/2's env of a local fun that captures nothing is the empty list",
            &[BeamAsm, OtpApps],
            "fun_local_env",
            "1",
        ),
        module_case0(
            "bif_extra/fun_info_local_captured_env",
            "fun_info/2's env of a local fun lists the captured value it closed over",
            &[BeamAsm, OtpApps],
            "fun_captured_env",
            "1",
        ),
        module_case0(
            "bif_extra/fun_info_local_pid_item_is_a_pid",
            "fun_info/2's pid item of a local fun is tagged `pid` and holds the creating \
             process's pid (classified, never rendered: a pid is per-VM-start)",
            &[BeamAsm, OtpApps],
            "fun_pid_item",
            "2",
        ),
        module_case0(
            "bif_extra/fun_info_local_whole_list",
            "the whole ten-key fun_info/1 proplist of a compiled local fun, with only the \
             volatile and hash values classified",
            &[BeamAsm, OtpApps],
            "fun_whole_list",
            "10",
        ),
        // --- the process_info/2 execution-position items of a spawned process (A.8) --------
        module_case0(
            "bif_extra/process_info_spawned_position_items",
            "process_info/2 of a spawned parked process answers current_function, initial_call, \
             catchlevel and status — the ledger's `unsupported` position items now resolve",
            &[BeamAsm, OtpApps],
            "pi_position",
            "4",
        ),
        module_case0(
            "bif_extra/process_info_spawned_current_location",
            "current_location of a spawned process names module/function/arity and carries the \
             {file,_}/{line,_} location list, the line being the parked child's own source line",
            &[BeamAsm, OtpApps],
            "pi_location",
            "5",
        ),
        module_case0(
            "bif_extra/process_info_spawned_current_stacktrace",
            "current_stacktrace of a spawned process is the single innermost frame, with a \
             two-entry location list",
            &[BeamAsm, OtpApps],
            "pi_stack",
            "1",
        ),
        // --- the process_info item set: values that are genuinely equal (A.8) --------------
        module_case0(
            "bif_extra/process_info_spawned_exact_item_values",
            "the twenty process_info/2 items whose value of a spawned parked process is exactly \
             the oracle's (the mailbox, the flags, the emulator's heap policy, the trace state)",
            &[BeamAsm, OtpApps],
            "pi_exact",
            "20",
        ),
        module_case0(
            "bif_extra/process_info_spawned_item_value_kinds",
            "the widest process_info/2 item set — all 31 items — is answered for a spawned \
             process with the oracle's value kind (an internal quantity is a kind, not a value)",
            &[BeamAsm, OtpApps],
            "pi_shape",
            "31",
        ),
        // --- the process_info/1 whole-list key set (A.8) -----------------------------------
        module_case0(
            "bif_extra/process_info_spawned_key_list",
            "process_info/1 of an unregistered spawned process is the fixed 16-key set in the \
             emulator's own order (registered_name is omitted)",
            &[BeamAsm, OtpApps],
            "pi_keys",
            "16",
        ),
        module_case0(
            "bif_extra/process_info_registered_key_list",
            "process_info/1 of a registered spawned process prepends registered_name to the \
             16-key set",
            &[BeamAsm, OtpApps],
            "pi_registered_keys",
            "17",
        ),
        // --- the mailbox / dictionary / link / monitor items (A.8) -------------------------
        module_case0(
            "bif_extra/process_info_messages_and_queue_length",
            "process_info/2's message_queue_len and messages report a spawned process's queued \
             payloads, in FIFO order",
            &[BeamAsm, OtpApps],
            "pi_messages",
            "2",
        ),
        module_case0(
            "bif_extra/process_info_dictionary_and_item_key_lookup",
            "process_info/2's dictionary item lists a spawned process's stored pairs (sorted: \
             the oracle's own order is a per-VM hash order) and the {dictionary,Key} item \
             answers the stored value or `undefined`",
            &[BeamAsm, OtpApps],
            "pi_dict",
            "3",
        ),
        module_case0(
            "bif_extra/process_info_links_round_trip",
            "process_info/2's links item follows link/1 and unlink/1 on a spawned process",
            &[BeamAsm, OtpApps],
            "pi_links",
            "3",
        ),
        module_case0(
            "bif_extra/process_info_monitors_of_a_spawned_process",
            "the monitored_by item of a monitored process and the monitors item of the monitor \
             both report the {process,_} pair (counted, never rendered: a pid is volatile)",
            &[BeamAsm, OtpApps],
            "pi_monitors",
            "2",
        ),
        module_case0(
            "bif_extra/process_info_dead_pid_answers_undefined",
            "process_info/2 and process_info/1 answer the atom `undefined` for a pid that is no \
             longer in the process table (never an empty list)",
            &[BeamAsm, OtpApps],
            "pi_dead",
            "2",
        ),
        module_case0(
            "bif_extra/process_info_refusals_are_badarg",
            "an unknown item name, a first argument that is not a pid and a non-atom/non-list \
             item all raise error:badarg",
            &[BeamAsm, OtpApps],
            "pi_badarg",
            "3",
        ),
        module_case0(
            "bif_extra/process_info_item_list_keeps_order_and_duplicates",
            "process_info/2 with an item *list* answers in the requested order and repeats a \
             duplicated item",
            &[BeamAsm, OtpApps],
            "pi_list_order",
            "3",
        ),
        // --- Module:module_info/1 of a compiled module (A.7) -------------------------------
        module_case(
            "bif_extra/module_info_exports_of_a_compiled_module",
            "Module:module_info(exports) of a freshly erlc-compiled module is the sorted export \
             table plus module_info/0,1",
            &[BeamAsm, OtpApps],
            "mod_info",
            1,
            &[0],
            "23",
        ),
        module_case(
            "bif_extra/module_info_module_attribute",
            "Module:module_info(module) of a compiled module answers its (atom) module name",
            &[BeamAsm, OtpApps],
            "mod_info",
            1,
            &[1],
            "1",
        ),
        module_case(
            "bif_extra/module_info_attributes_carry_the_vsn",
            "Module:module_info(attributes) of a compiled module carries exactly the \
             compiler-added `vsn` attribute, as an integer (the md5 itself is \
             compiler-version-derived, so only its presence and type are pinned)",
            &[BeamAsm, OtpApps],
            "mod_info",
            1,
            &[2],
            "2",
        ),
        // --- the erlang:* BIF / introspection shapes of the *running* process (A.7/A.8) ----
        info_case(
            "bif_extra/process_info_self_registered_name_is_the_empty_list",
            "process_info(self(), registered_name) of an unregistered process is the bare empty \
             list (the /2 atom form is deliberately unwrapped)",
            "erlang:process_info(self(), registered_name)",
            "ok []",
        ),
        info_case(
            "bif_extra/process_info_self_registered_name_in_the_item_list_form",
            "the *list* form of process_info/2 wraps registered_name — {registered_name,[]} for \
             an unregistered process — and reports status alongside it",
            "erlang:process_info(self(), [registered_name, status])",
            "ok [{registered_name,[]},{status,running}]",
        ),
        info_case(
            "bif_extra/process_info_self_whole_list_key_order",
            "process_info/1 of the running process is the same fixed 16-key set in the \
             emulator's own order (registered_name omitted while unregistered)",
            "[K || {K, _} <- erlang:process_info(self())]",
            "ok [current_function,initial_call,status,message_queue_len,links,dictionary,\
             trap_exit,error_handler,priority,group_leader,total_heap_size,heap_size,stack_size,\
             reductions,garbage_collection,suspending]",
        ),
        info_case(
            "bif_extra/process_info_self_position_items",
            "current_function, initial_call and status of the *running* `-eval` process: the \
             evaluator's own frame, the root apply/2 initial call and the `running` status",
            "{erlang:process_info(self(), current_function), \
             erlang:process_info(self(), initial_call), erlang:process_info(self(), status)}",
            "ok {{current_function,{erl_eval,do_apply,7}},{initial_call,{erlang,apply,2}},\
             {status,running}}",
        ),
        info_case(
            "bif_extra/process_info_self_trap_exit_round_trip",
            "process_info(self(), trap_exit) tracks process_flag(trap_exit, _) in both \
             directions",
            "begin process_flag(trap_exit, true), \
             T = erlang:process_info(self(), trap_exit), \
             process_flag(trap_exit, false), \
             {T, erlang:process_info(self(), trap_exit)} end",
            "ok {{trap_exit,true},{trap_exit,false}}",
        ),
        info_case(
            "bif_extra/process_info_self_dictionary_after_put",
            "process_info(self(), dictionary) lists the calling process's stored pair (a single \
             entry, so its order is vacuous)",
            "begin put(diff_k, diff_v), \
             erlang:process_info(self(), dictionary) end",
            "ok {dictionary,[{diff_k,diff_v}]}",
        ),
        info_case(
            "bif_extra/process_info_self_last_calls_without_a_buffer",
            "process_info/2's last_calls of a process with no saved-calls buffer is the atom \
             `false` — both for the default (never armed) state and after an explicit \
             process_flag(save_calls, 0), which is the only last_calls state this runtime \
             answers (an armed buffer's list of recorded calls is a runtime-side gap)",
            "{erlang:process_info(self(), last_calls), \
             begin process_flag(save_calls, 0), \
             erlang:process_info(self(), last_calls) end}",
            "ok {{last_calls,false},{last_calls,false}}",
        ),
        bif_case(
            "bif_extra/function_exported_after_ensure_loaded",
            "function_exported/3 answers true for an exported function and false for an \
             unexported one once the module is explicitly loaded",
            "begin code:ensure_loaded(lists), \
             {erlang:function_exported(lists, reverse, 1), \
              erlang:function_exported(lists, no_such_fun, 0)} end",
            "ok {true,false}",
        ),
        bif_case(
            "bif_extra/is_builtin_of_a_declared_and_a_bogus_bif",
            "is_builtin/3 answers true for a declared erlang:* BIF and false for an undeclared \
             name",
            "{erlang:is_builtin(erlang, self, 0), erlang:is_builtin(erlang, not_a_bif, 0)}",
            "ok {true,false}",
        ),
        bif_case(
            "bif_extra/get_module_info_module_attribute",
            "erlang:get_module_info/2 (the deprecated module_info/1 alias) answers the module \
             name of the erlang module",
            "erlang:get_module_info(erlang, module)",
            "ok erlang",
        ),
        bif_case(
            "bif_extra/check_old_code_of_a_loaded_module",
            "check_old_code/1 answers false for a module with no old-code version",
            "begin code:ensure_loaded(lists), erlang:check_old_code(lists) end",
            "ok false",
        ),
        // --- erlang:display/1 — the C %T renderer and its CRLF terminator (A.7) -----------
        bif_case(
            "bif_extra/display_atom_and_integer_are_bare_and_crlf_terminated",
            "erlang:display/1 writes an atom and an integer bare through the C `%T` printer, \
             one `\\r\\n`-terminated line per call, and the probe's own line follows them",
            "begin erlang:display(hello), erlang:display(42), done end",
            "hello\r\n42\r\nok done",
        ),
        bif_case(
            "bif_extra/display_tuple_quotes_a_printable_list_element",
            "erlang:display/1 renders a tuple elementwise and quotes a printable list element \
             (`\"s\"`), while the empty list stays `[]`",
            "begin erlang:display({a,1,\"s\",[]}), done end",
            "{a,1,\"s\",[]}\r\nok done",
        ),
        bif_case(
            "bif_extra/display_list_printable_elements_become_a_quoted_string",
            "erlang:display/1 renders an all-printable list as its quoted string form (`\"hi\"`) \
             and an improper list elementwise with the `|` tail marker",
            "begin erlang:display([104,105]), erlang:display([1,2,3|tail]), done end",
            "\"hi\"\r\n[1,2,3|tail]\r\nok done",
        ),
        bif_case(
            "bif_extra/display_binary_quotes_printable_bytes_and_numbers_the_rest",
            "erlang:display/1 renders a printable binary in its quoted `<<\"…\">>` form and a \
             binary with a non-printable byte in the numbered `<<1,2,255>>` form",
            "begin erlang:display(<<\"bin\">>), erlang:display(<<1,2,255>>), done end",
            "<<\"bin\">>\r\n<<1,2,255>>\r\nok done",
        ),
        bif_case(
            "bif_extra/display_float_uses_the_percent_t_exponent_format",
            "erlang:display/1 renders a float with the `%T` printer's `%e` shape \
             (`1.500000e+00`, not `1.5`) and a negative integer with its sign",
            "begin erlang:display(1.5), erlang:display(-42), done end",
            "1.500000e+00\r\n-42\r\nok done",
        ),
        bif_case(
            "bif_extra/display_quotes_an_atom_that_needs_it_and_renders_the_empty_tuple",
            "erlang:display/1 quotes an atom that is not a plain lowercase name \
             (`'hello world'`) and renders the empty tuple as `{}`",
            "begin erlang:display('hello world'), erlang:display({}), done end",
            "'hello world'\r\n{}\r\nok done",
        ),
        bif_case(
            "bif_extra/display_nests_without_a_pretty_print_depth_limit",
            "erlang:display/1's `%T` printer has no `~p`-style depth or length limit: a \
             seven-deep tuple and a list with a non-printable element both render in full",
            "begin erlang:display({1,{2,{3,{4,{5,{6,{7,8}}}}}}}), \
             erlang:display([97,98,300]), done end",
            "{1,{2,{3,{4,{5,{6,{7,8}}}}}}}\r\n[97,98,300]\r\nok done",
        ),
        bif_case(
            "bif_extra/display_returns_true_on_every_call",
            "erlang:display/1 answers the atom `true` (not the term), so two calls in one \
             expression render both lines and yield `{true,true}`",
            "{erlang:display(one), erlang:display(two)}",
            "one\r\ntwo\r\nok {true,true}",
        ),
        // --- erts_internal:term_to_string/1,2 — the same %T printer, budgeted (A.7) --------
        bif_case(
            "bif_extra/term_to_string_of_a_tuple_is_a_character_list",
            "erts_internal:term_to_string/1 answers a character list (a list, never a binary) \
             holding the `%T` rendering — `{a,1}` renders the five bytes `\"{a,1}\"`",
            "{is_list(erts_internal:term_to_string({a,1})), \
             is_binary(erts_internal:term_to_string({a,1})), \
             erts_internal:term_to_string({a,1})}",
            "ok {true,false,[123,97,44,49,125]}",
        ),
        bif_case(
            "bif_extra/term_to_string_renders_every_term_kind_with_percent_t",
            "erts_internal:term_to_string/1 renders a float, a map, a printable list and a \
             printable binary exactly like the `%T` printer (the expected strings are the \
             oracle's measured renderings)",
            TERM_TO_STRING_KINDS,
            "ok [true,true,true,true]",
        ),
        bif_case(
            "bif_extra/term_to_string_limit_arms_and_the_truncation_marker",
            "erts_internal:term_to_string/2 takes a byte budget: an affordable term still gains \
             the `\"... \"` marker, a tight budget truncates mid-term, and `undefined` / \
             `INT_MAX-1` are the unlimited arms",
            TERM_TO_STRING_LIMITS,
            "ok [true,true,true,true]",
        ),
        bif_case(
            "bif_extra/term_to_string_limit_outside_the_accepted_arms_is_badarg",
            "erts_internal:term_to_string/2's argument check accepts only `1 < Limit < INT_MAX` \
             or the atom `undefined`, so 1, 0, -1, `INT_MAX` itself and a non-integer limit all \
             raise error:badarg",
            TERM_TO_STRING_BADARG_MATRIX,
            "ok [{error,badarg},{error,badarg},{error,badarg},{error,badarg},{error,badarg}]",
        ),
        bif_case(
            "bif_extra/term_to_string_of_the_wrong_arity_is_undef",
            "erts_internal:term_to_string/0 is no exported function, so the call raises the \
             oracle's bare error:undef",
            "try erts_internal:term_to_string() of _ -> ok catch Cls:Rs -> {Cls, Rs} end",
            "ok {error,undef}",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case id, in declaration order — so a rename is a single edit and a missing case
    /// fails a test.
    const IDS: &[&str] = &[
        "bif_extra/fun_info_local_identity_items",
        "bif_extra/fun_info_local_index_and_hash_shape_items",
        "bif_extra/fun_info_local_env_is_empty",
        "bif_extra/fun_info_local_captured_env",
        "bif_extra/fun_info_local_pid_item_is_a_pid",
        "bif_extra/fun_info_local_whole_list",
        "bif_extra/process_info_spawned_position_items",
        "bif_extra/process_info_spawned_current_location",
        "bif_extra/process_info_spawned_current_stacktrace",
        "bif_extra/process_info_spawned_exact_item_values",
        "bif_extra/process_info_spawned_item_value_kinds",
        "bif_extra/process_info_spawned_key_list",
        "bif_extra/process_info_registered_key_list",
        "bif_extra/process_info_messages_and_queue_length",
        "bif_extra/process_info_dictionary_and_item_key_lookup",
        "bif_extra/process_info_links_round_trip",
        "bif_extra/process_info_monitors_of_a_spawned_process",
        "bif_extra/process_info_dead_pid_answers_undefined",
        "bif_extra/process_info_refusals_are_badarg",
        "bif_extra/process_info_item_list_keeps_order_and_duplicates",
        "bif_extra/module_info_exports_of_a_compiled_module",
        "bif_extra/module_info_module_attribute",
        "bif_extra/module_info_attributes_carry_the_vsn",
        "bif_extra/process_info_self_registered_name_is_the_empty_list",
        "bif_extra/process_info_self_registered_name_in_the_item_list_form",
        "bif_extra/process_info_self_whole_list_key_order",
        "bif_extra/process_info_self_position_items",
        "bif_extra/process_info_self_trap_exit_round_trip",
        "bif_extra/process_info_self_dictionary_after_put",
        "bif_extra/process_info_self_last_calls_without_a_buffer",
        "bif_extra/function_exported_after_ensure_loaded",
        "bif_extra/is_builtin_of_a_declared_and_a_bogus_bif",
        "bif_extra/get_module_info_module_attribute",
        "bif_extra/check_old_code_of_a_loaded_module",
        "bif_extra/display_atom_and_integer_are_bare_and_crlf_terminated",
        "bif_extra/display_tuple_quotes_a_printable_list_element",
        "bif_extra/display_list_printable_elements_become_a_quoted_string",
        "bif_extra/display_binary_quotes_printable_bytes_and_numbers_the_rest",
        "bif_extra/display_float_uses_the_percent_t_exponent_format",
        "bif_extra/display_quotes_an_atom_that_needs_it_and_renders_the_empty_tuple",
        "bif_extra/display_nests_without_a_pretty_print_depth_limit",
        "bif_extra/display_returns_true_on_every_call",
        "bif_extra/term_to_string_of_a_tuple_is_a_character_list",
        "bif_extra/term_to_string_renders_every_term_kind_with_percent_t",
        "bif_extra/term_to_string_limit_arms_and_the_truncation_marker",
        "bif_extra/term_to_string_limit_outside_the_accepted_arms_is_badarg",
        "bif_extra/term_to_string_of_the_wrong_arity_is_undef",
    ];

    /// The `process_info/2` items the shape case must keep covering (the "widest item set").
    const ITEMS: &[&str] = &[
        "status",
        "message_queue_len",
        "links",
        "monitors",
        "monitored_by",
        "dictionary",
        "trap_exit",
        "error_handler",
        "priority",
        "group_leader",
        "current_function",
        "initial_call",
        "current_location",
        "current_stacktrace",
        "catchlevel",
        "total_heap_size",
        "heap_size",
        "stack_size",
        "reductions",
        "garbage_collection",
        "min_heap_size",
        "min_bin_vheap_size",
        "fullsweep_after",
        "max_heap_size",
        "message_queue_data",
        "trace",
        "sequential_trace_token",
        "suspending",
        "last_calls",
        "memory",
        "binary",
    ];

    /// The keys of the `fun_info/1` proplist, i.e. the whole local-fun identity surface.
    const FUN_INFO_KEYS: &[&str] = &[
        "arity", "env", "index", "module", "name", "new_index", "new_uniq", "pid", "type", "uniq",
    ];

    /// The line of [`SOURCE`] the parked child sits in — `pi_location/0`'s expected line. It is
    /// the first line carrying the child's parked `receive`.
    fn parked_child_line() -> usize {
        SOURCE
            .lines()
            .position(|line| line.contains("receive stop -> ok end"))
            .map(|idx| idx + 1)
            .expect("SOURCE must keep the parked child's `receive`")
    }

    fn find(id: &str) -> Case {
        group()
            .into_iter()
            .find(|case| case.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the bif_extra group"))
    }

    #[test]
    fn the_group_has_the_expected_size_and_ids() {
        let cases = group();
        assert_eq!(cases.len(), IDS.len(), "the group's size is pinned");
        for (case, id) in cases.iter().zip(IDS) {
            assert_eq!(case.id, *id, "the group's case order is pinned");
        }
        let mut sorted: Vec<&str> = IDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), IDS.len(), "the ids are unique");
    }

    #[test]
    fn every_case_asserts_stdout_and_exits_zero() {
        for case in group() {
            assert_eq!(
                case.golden.exit_code, 0,
                "case {}: every case here agrees, so every golden exits 0",
                case.id
            );
            assert!(
                case.golden.stdout.is_some_and(|line| !line.is_empty()),
                "case {}: every case pins a stdout line",
                case.id
            );
        }
    }

    /// The probe wrapper binds `V`, `C`, `R` and `X`, so an expression that binds one of those
    /// names makes the **oracle** fail to compile the wrapped probe — a guaranteed, silent
    /// false mismatch.
    #[test]
    fn no_eval_case_binds_a_probe_wrapper_variable() {
        for case in group() {
            let Some(expr) = case.expr() else { continue };
            let names: Vec<&str> = expr
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .filter(|token| !token.is_empty())
                .collect();
            for bound in ["V", "C", "R", "X"] {
                assert!(
                    !names.contains(&bound),
                    "case {} binds `{bound}`, which the probe wrapper already binds",
                    case.id
                );
            }
        }
    }

    /// Nothing in this group may block: no `timer:sleep/1` anywhere, no unbounded `receive` in an
    /// expression, and — in the embedded module — every `receive` other than the parked child's
    /// own `receive stop -> ok end` must carry an `after` arm.
    #[test]
    fn no_case_waits_on_a_message_without_a_timeout() {
        assert!(
            !SOURCE.contains("timer:sleep"),
            "the embedded module must not sleep"
        );
        for (idx, line) in SOURCE.lines().enumerate() {
            if !line.contains("receive") {
                continue;
            }
            assert!(
                line.contains(" after ") || line.contains("receive stop -> ok end"),
                "SOURCE line {}: an unbounded receive that is not the parked child: {line:?}",
                idx + 1
            );
        }
        for case in group() {
            let Some(expr) = case.expr() else { continue };
            assert!(
                !expr.contains("timer:sleep"),
                "case {}: `timer:sleep` blocks in the Rust runtime",
                case.id
            );
            assert!(
                !expr.contains("receive") || expr.contains(" after "),
                "case {}: a `receive` must carry a bounded `after` arm",
                case.id
            );
        }
    }

    /// A golden must never embed a per-VM-start value or a per-VM-unstable rendering.
    #[test]
    fn no_golden_pins_a_volatile_value() {
        for case in group() {
            let golden = case.golden.stdout.expect("every case pins stdout");
            for volatile in ["<0.", "#Ref<", "#Fun<", "#Port<", "#{"] {
                assert!(
                    !golden.contains(volatile),
                    "case {}: the golden pins {volatile:?}: {golden:?}",
                    case.id
                );
            }
        }
    }

    /// Every module-run case must actually run [`SOURCE`] with no NIF fixture, an arity that
    /// matches its argument list, an integer golden and the `.beam` contract.
    #[test]
    fn every_module_case_runs_the_embedded_source() {
        let cases = group();
        let modules: Vec<&Case> = cases
            .iter()
            .filter(|case| matches!(case.kind, CaseKind::Module(_)))
            .collect();
        assert_eq!(modules.len(), 23, "the group's module-run half is pinned");
        for case in modules {
            let CaseKind::Module(run) = case.kind else {
                unreachable!("filtered above")
            };
            assert_eq!(run.module, MODULE, "case {}", case.id);
            assert_eq!(run.source, SOURCE, "case {}", case.id);
            assert_eq!(
                run.arity as usize,
                run.args.len(),
                "case {}: arity must match the argument list",
                case.id
            );
            assert!(run.nif.is_none(), "case {} needs no C fixture", case.id);
            assert!(
                case.contracts.contains(&Contract::BeamAsm),
                "case {}: a module-run case evidences the .beam contract",
                case.id
            );
            let golden = case.golden.stdout.expect("every case pins stdout");
            assert!(
                golden.chars().all(|ch| ch.is_ascii_digit()),
                "case {}: a module-run golden is a plain integer line: {golden:?}",
                case.id
            );
        }
    }

    /// `pi_location/0`'s expectation carries the parked child's **source line**, i.e. a function
    /// of [`SOURCE`]'s line layout: recompute it so an edit that shifts the source fails the
    /// build instead of silently turning the case into a partial match.
    #[test]
    fn the_location_expectation_names_the_parked_childs_source_line() {
        let expected = format!("[file, line], {}]", parked_child_line());
        assert!(
            SOURCE.contains(&expected),
            "pi_location/0's expectation must name the parked child's SOURCE line {}: \
             `{expected}` is missing",
            parked_child_line()
        );
        assert_eq!(
            find("bif_extra/process_info_spawned_current_location")
                .golden
                .stdout,
            Some("5"),
            "the location case checks five components (module, function, arity, the location \
             keys and the line)"
        );
    }

    /// The shape case must keep covering the *whole* documented `process_info/2` item set: this
    /// is the list the group's doc calls "the widest item set", and shrinking it silently would
    /// hide a regression.
    #[test]
    fn the_shape_case_covers_the_whole_item_set() {
        for item in ITEMS {
            assert!(
                SOURCE.contains(item),
                "the embedded module must keep probing the `{item}` item"
            );
        }
        assert_eq!(ITEMS.len(), 31, "the widest item set is the 31-item one");
        assert_eq!(
            find("bif_extra/process_info_spawned_item_value_kinds")
                .golden
                .stdout,
            Some("31"),
            "the shape case checks one kind per item"
        );
    }

    /// The local-fun identity surface must be exercised through **all** of `fun_info`'s ten keys
    /// — that is the whole point of the group (the ledger recorded the surface as exit-3).
    #[test]
    fn the_fun_info_cases_cover_every_key_of_the_proplist() {
        assert_eq!(FUN_INFO_KEYS.len(), 10, "fun_info/1's ten keys");
        for key in FUN_INFO_KEYS {
            assert!(
                SOURCE.contains(&format!("{{{key}, ")) || SOURCE.contains(&format!("{{{key},<")),
                "the embedded module must keep probing the `{key}` key"
            );
        }
        assert_eq!(
            find("bif_extra/fun_info_local_whole_list").golden.stdout,
            Some("10"),
            "the whole-list case checks one key per entry"
        );
    }

    /// The `erlang:display/1` cases are the group's **renderer** half: each one must pin the
    /// `%T` rendering *and* the `\r\n` its `write_f_add_cr` appends, and each must keep the
    /// probe's own line last (a golden is the whole probe stdout minus the final newline, so
    /// the pre-printed text is part of it — see the module docs).
    #[test]
    fn every_display_case_pins_the_crlf_terminator_and_the_probe_line() {
        let cases = group();
        let display: Vec<&Case> = cases
            .iter()
            .filter(|case| case.id.starts_with("bif_extra/display_"))
            .collect();
        assert!(
            display.len() >= 8,
            "the renderer half must keep its eight display cases, got {}",
            display.len()
        );
        for case in display {
            let golden = case.golden.stdout.expect("stdout");
            assert!(
                golden.contains("\r\n"),
                "case {}: a display/1 line is CRLF-terminated: {golden:?}",
                case.id
            );
            let probe_line = golden.rsplit('\n').next().unwrap_or(golden);
            assert!(
                probe_line.starts_with("ok "),
                "case {}: the probe's own line comes last: {golden:?}",
                case.id
            );
            assert_eq!(
                case.kind_label(),
                "value",
                "case {}: a renderer case pins the `~w` rendering, not an ETF image",
                case.id
            );
        }
    }

    /// The renderer half must cover the term kinds whose `%T` quoting rules differ — a bare
    /// atom/integer, a tuple, a printable list, a binary, a float and a quoted atom. Each is
    /// named by the term it displays, so a rewrite that drops one fails here.
    #[test]
    fn the_display_cases_cover_every_quoting_rule() {
        let exprs: Vec<&str> = group()
            .into_iter()
            .filter(|case| case.id.starts_with("bif_extra/display_"))
            .filter_map(|case| case.expr())
            .collect();
        for displayed in [
            "erlang:display(hello)",
            "erlang:display(42)",
            "erlang:display({a,1,\"s\",[]})",
            "erlang:display([104,105])",
            "erlang:display([1,2,3|tail])",
            "erlang:display(<<\"bin\">>)",
            "erlang:display(<<1,2,255>>)",
            "erlang:display(1.5)",
            "erlang:display('hello world')",
            "erlang:display({})",
        ] {
            assert!(
                exprs.iter().any(|expr| expr.contains(displayed)),
                "no display case pins `{displayed}`"
            );
        }
    }

    /// The `erts_internal:term_to_string/1,2` cases must keep pinning all three arms the C has:
    /// the `/1` form (a character list), the `/2` budget arms (including the truncation marker)
    /// and the `/2` argument check (`1 < L < INT_MAX` or `undefined`, else `badarg`).
    #[test]
    fn the_term_to_string_cases_cover_the_three_arms() {
        assert!(TERM_TO_STRING_KINDS.contains("term_to_string(T) =:="));
        assert!(TERM_TO_STRING_LIMITS.contains("\"{a,1}... \""));
        assert!(TERM_TO_STRING_LIMITS.contains("\"{a... \""));
        assert!(TERM_TO_STRING_LIMITS.contains("undefined"));
        assert!(TERM_TO_STRING_BADARG_MATRIX.contains("2147483647"));
        assert_eq!(
            find("bif_extra/term_to_string_of_a_tuple_is_a_character_list")
                .golden
                .stdout,
            Some("ok {true,false,[123,97,44,49,125]}"),
            "the /1 case pins the character list and its exact bytes (`\"{{a,1}}\"`)"
        );
        assert_eq!(
            find("bif_extra/term_to_string_limit_outside_the_accepted_arms_is_badarg")
                .golden
                .stdout,
            Some("ok [{error,badarg},{error,badarg},{error,badarg},{error,badarg},{error,badarg}]"),
            "one badarg per rejected limit arm"
        );
    }

    /// A capability gap must never be recorded here: every case in this group is an agreement.
    #[test]
    fn no_case_records_a_capability_gap() {
        for case in group() {
            let golden = case.golden.stdout.expect("stdout");
            assert!(
                !golden.starts_with("unsupported")
                    && !golden.contains("unsupported:")
                    && !golden.contains("unsupported_"),
                "case {}: a gap does not belong in this group: {golden:?}",
                case.id
            );
            assert!(
                !golden.starts_with("exception"),
                "case {}: every case here evaluates to a value: {golden:?}",
                case.id
            );
        }
    }
}
