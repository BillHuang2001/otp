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

//! Contract **A.9** — build / bootstrapping observability.
//!
//! A.9 is "the system can build and start itself". Its black-box residue is what the
//! *booted* system exposes about itself: the version banner, the **code server** (the
//! code path, `code:which/1`/`code:lib_dir/1` resolution, and the set of loaded modules),
//! and the compiled-artifact extension. Every assertion here is **structural**
//! (`is_list`, `is_atom`, …) rather than by exact value wherever the value is
//! machine-dependent — the oracle is OTP 29 while the rewrite targets OTP 30, and the
//! code path is installation-specific. What both runtimes must share is the *shape*;
//! only the values that are stable across releases and installations (`non_existing`,
//! `preloaded`, `false`, `undefined`, `{module,Module}`, `{error,bad_name}`, `".beam"`) are
//! asserted exactly.
//!
//! Beyond the code path and loaded-module registry, the group also pins the **code
//! server's query surface** (`code:is_loaded/1`, `code:ensure_loaded/1`, `code:is_sticky/1`,
//! `code:get_object_code/1`, `code:lib_dir/2`) and the **OTP application surface** the booted
//! system exposes (`application:get_env/1,2,3`, `get_all_env/1`, `loaded_applications/0`,
//! `which_applications/0`, `get_key/2`) — again by shape wherever the value is
//! release/installation-specific. `code:get_object_code/1` is pinned as a *relation* to
//! `code:which/1` and `code:objfile_extension/0` (a path is never asserted by value), and the
//! code path is pinned by resolving the whole booted stdlib floor through `code:ensure_loaded/1`.
//!
//! The **process** surface is driven through `-eval` and asserted **by shape**, never by an
//! exact pid (pids vary per VM start): `spawn/1`, `spawn_link/1` and `spawn_opt/2` must return
//! a pid, the mailbox carries a real `spawn` + `!` + `receive` round trip (including a
//! non-zero `after` timeout), `erlang:processes/0` is a list of pids whose length is the
//! bootstrap's own `system_info(process_count)`, and `erlang:registered/0` is the booted
//! system's name table (a name the boot chain owns — `init` — cannot be re-registered). The
//! **process dictionary** (`put/2`, `get/1`, `erase/1`) and `init:get_status/0` are part of the
//! booted system's observable surface.
//!
//! The boot-path **prerequisite primitives** (the `erlang:pre_loaded/0` set, the boot script's
//! `which_applications/0`/`loaded_applications/0`, the preloaded `init`, kernel's
//! `error_logger`/`vsn` entries, the `register/2` + `whereis/1` and group-leader-inheritance
//! kernel-process primitives, the `-boot` init argument and the boot chain's
//! `proc_lib:start_link/4` start wait) live in the sibling `boot_prereq`
//! group; this group keeps the boot/version observability surface.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.9 boot-observability case: evaluate `expr` under both runtimes with the same
/// `-eval` argv and pin the oracle's `~w` rendering.
fn eval(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::Build],
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

/// The A.9 cases: boot/version observability, asserted by shape.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "boot/system_version_is_list",
            description: "erlang:system_info(system_version) is a string (shape, not value)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(erlang:system_info(system_version))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/erts_version_is_string",
            description: "erlang:system_info(version) is a string (the erts version banner shape)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(erlang:system_info(version))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_path_is_list",
            description: "code:get_path/0 is a list of directory entries",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:get_path())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_path_is_non_empty",
            description: "a booted system has a non-empty code path (it can resolve modules at all)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:get_path() =/= []",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_path_entries_are_strings",
            description: "every code:get_path/0 entry is a non-empty string (a directory path)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:all(fun(S) -> is_list(S) andalso S =/= [] end, code:get_path())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_path_entries_are_printable",
            description: "every code:get_path/0 entry is a printable string (a list of bytes)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:all(fun(S) -> lists:all(fun erlang:is_integer/1, S) end, code:get_path())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/which_loaded_module_is_path",
            description: "code:which/1 resolves an installed module to a string path",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:which(lists))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/which_missing_module_is_nonexisting",
            description: "code:which/1 reports the atom `non_existing` for an unknown module",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:which(a_definitely_nonexistent_module_xyz)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok non_existing"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/which_preloaded_module_is_preloaded",
            description: "code:which/1 reports the atom `preloaded` for a boot-registry module",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:which(erlang)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok preloaded"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/lib_dir_of_stdlib_is_string",
            description: "code:lib_dir/1 resolves an installed application to a string directory",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:lib_dir(stdlib))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/lib_dir_root_is_string",
            description: "code:lib_dir/0 is a string (the installation's library root)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:lib_dir())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/lib_dir_of_missing_app_is_bad_name",
            description: "code:lib_dir/1 reports {error,bad_name} for an unknown application",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:lib_dir(no_such_application_xyz)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {error,bad_name}"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/all_loaded_is_list",
            description: "code:all_loaded/0 is a list of {Module, Beam} pairs",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:all_loaded())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/all_loaded_is_non_empty",
            description: "the booted system has at least one loaded module",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "length(code:all_loaded()) > 0",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/loaded_entry_is_tagged_tuple",
            description: "every code:all_loaded/0 entry is tagged by a module atom",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_atom(element(1, hd(code:all_loaded())))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/all_loaded_entries_are_module_tagged",
            description: "every code:all_loaded/0 entry is tagged by a module atom, not just the first",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:all(fun(X) -> is_atom(element(1, X)) end, code:all_loaded())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/all_loaded_contains_preloaded_erlang",
            description: "the preloaded `erlang` module is present in the loaded-module registry",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:member(erlang, [Mod || {Mod, _} <- code:all_loaded()])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/objfile_extension_is_beam",
            description: "code:objfile_extension/0 is the compiled-module extension \".beam\"",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:objfile_extension() =:= \".beam\"",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- Code server: is_loaded / ensure_loaded / is_sticky -------------------------
        Case {
            id: "boot/code_is_loaded_shape",
            description: "code:is_loaded/1 reports {file,Path} for a loaded module (shape, not the path)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                // `code:is_loaded/1` answers from the code server's *load state*, not from
                // code-path resolvability, so the case must make that state explicit rather
                // than inherit the oracle's boot set: the Rust runtime boots a smaller module
                // set than the oracle and `lists` is not preloaded (it is, however, on the
                // code path, so `code:ensure_loaded/1` succeeds on both sides). Same fix as
                // the sibling `boot_extra/code_is_loaded_lists_is_a_file_pair`.
                expr: "begin code:ensure_loaded(lists), L = code:is_loaded(lists), is_tuple(L) andalso element(1, L) =:= file andalso is_list(element(2, L)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_is_loaded_unknown_is_false",
            description: "code:is_loaded/1 reports the atom `false` for an unknown module",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:is_loaded(a_definitely_nonexistent_module_xyz)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_ensure_loaded_loadable",
            description: "code:ensure_loaded/1 loads a known module and reports {module,Module}",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:ensure_loaded(lists)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {module,lists}"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_ensure_loaded_unknown_is_error",
            description: "code:ensure_loaded/1 reports an {error,Reason} shape for an unloadable module",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Loaded = code:ensure_loaded(a_definitely_nonexistent_module_xyz), is_tuple(Loaded) andalso element(1, Loaded) =:= error end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_is_sticky_erlang",
            description: "code:is_sticky/1 on the preloaded `erlang` module is a boolean",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_boolean(code:is_sticky(erlang))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_is_sticky_kernel",
            description: "code:is_sticky/1 on the booted `kernel` module is a boolean",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_boolean(code:is_sticky(kernel))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_is_sticky_2_arity_is_undef",
            description: "the code module exports no is_sticky/2, so the call raises error:undef",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:is_sticky(erlang, preloaded)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error undef"),
                exit_code: 1,
            },
        },
        // --- OTP application surface: get_env / get_all_env / loaded_applications / get_key
        Case {
            id: "boot/application_get_env_1_shape",
            description: "application:get_env/1 for kernel is `undefined` or {ok,Value} (shape, not a machine value)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Env = application:get_env(kernel), Env =:= undefined orelse (is_tuple(Env) andalso element(1, Env) =:= ok) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_env_2_shape",
            description: "application:get_env/2 for kernel is `undefined` or {ok,Value} (shape, not a machine value)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Env = application:get_env(kernel, a_nonexistent_key_xyz), Env =:= undefined orelse (is_tuple(Env) andalso element(1, Env) =:= ok) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_env_3_default",
            description: "application:get_env/3 returns the supplied default for an unset key",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "application:get_env(kernel, a_nonexistent_key_xyz, default_value)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok default_value"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_all_env_is_list",
            description: "application:get_all_env/1 for kernel is a list of {Key,Value} pairs",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(application:get_all_env(kernel))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_loaded_applications_shape",
            description: "application:loaded_applications/0 is a list of {App,Description,Vsn} triples",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:all(fun(X) -> is_tuple(X) andalso tuple_size(X) =:= 3 end, application:loaded_applications())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_which_applications_shape",
            description: "application:which_applications/0 is a list of {App,Description,Vsn} triples",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:all(fun(X) -> is_tuple(X) andalso tuple_size(X) =:= 3 end, application:which_applications())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_key_shape",
            description: "application:get_key/2 for a present key returns {ok,Value} (shape, not the version)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Key = application:get_key(kernel, vsn), is_tuple(Key) andalso element(1, Key) =:= ok andalso is_list(element(2, Key)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_key_missing_is_undefined",
            description: "application:get_key/2 reports `undefined` for a key the application does not set",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "application:get_key(kernel, a_nonexistent_key_xyz)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok undefined"),
                exit_code: 0,
            },
        },
        // --- Process surface: spawn + mailbox round trip, asserted by shape (never a pid) --
        Case {
            id: "boot/receive_message_from_spawned_process",
            description: "a spawned process sends its parent an atom, which the parent receives (spawn+receive round trip)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), spawn(fun() -> Self ! hello end), receive M -> M after 1000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok hello"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/receive_computed_value_from_spawned_process",
            description: "a spawned process computes and sends a value, which the parent receives",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), spawn(fun() -> Self ! 21 * 2 end), receive M -> M after 1000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/receive_pattern_match_from_spawned_process",
            description: "the parent selectively receives the pid inside a message tuple",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), spawn(fun() -> Self ! {self(), ok} end), receive {From, _} -> From =/= Self after 1000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/receive_ping_pong_between_processes",
            description: "two processes exchange a request and a reply through the mailbox",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), P = spawn(fun() -> receive {ping, From} -> From ! pong end end), P ! {ping, Self}, receive M -> M after 1000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok pong"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawn_link_notifies_caller",
            description: "spawn_link/1 spawns a process that notifies its parent through the mailbox",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), spawn_link(fun() -> Self ! linked end), receive M -> M after 1000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok linked"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawn_returns_pid",
            description: "spawn/1 returns a pid (shape only — the pid value varies per VM start)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_pid(spawn(fun() -> ok end))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawn_link_returns_pid",
            description: "spawn_link/1 returns a pid (shape only — the pid value varies per VM start)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_pid(spawn_link(fun() -> ok end))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawn_opt_returns_pid",
            description: "spawn_opt/2 with options returns a pid (shape only)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_pid(spawn_opt(fun() -> ok end, [link]))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawn_opt_empty_options_returns_pid",
            description: "spawn_opt/2 with an empty option list returns a pid (shape only)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_pid(spawn_opt(fun() -> ok end, []))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawned_process_round_trip",
            description: "a spawned process replies with a tagged tuple the parent selectively receives",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), spawn(fun() -> Self ! {roundtrip, ok} end), receive {roundtrip, Value} -> Value after 50 -> timeout end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,111,107>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/receive_after_timeout",
            description: "a receive with a non-zero `after` timeout and an empty mailbox yields `timeout`",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "receive Msg -> Msg after 50 -> timeout end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,7,116,105,109,101,111,117,116>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/receive_after_message_arrives",
            description: "a queued message is received before the `after` timeout expires",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin self() ! hi, receive Msg -> Msg after 5000 -> timeout end end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,104,105>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/processes_is_list",
            description: "erlang:processes/0 is a list of pids (shape, not the contents)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(erlang:processes())",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/processes_contains_self",
            description: "erlang:processes/0 contains the running process's own pid",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:member(self(), erlang:processes())",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        // --- The process dictionary and the boot-status surface -------------------------
        Case {
            id: "boot/process_dictionary_put_get",
            description: "put/2 stores a value that get/1 returns (the process dictionary)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin put(k, 1), get(k) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/process_dictionary_get_missing",
            description: "get/1 of an unset key yields the atom `undefined`",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "get(a_nonexistent_key_xyz)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,9,117,110,100,101,102,105,110,101,100>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/process_dictionary_erase",
            description: "erase/1 removes a key so a later get/1 yields `undefined`",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin put(k, 1), erase(k), get(k) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,9,117,110,100,101,102,105,110,101,100>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/init_get_status_is_tuple",
            description: "init:get_status/0 is the {State,Started} boot-status tuple",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_tuple(init:get_status())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/root_dir_is_string",
            description: "code:root_dir/0 is a string (the installation root the system booted from)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:root_dir())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- The code server's object-file resolution (the boot chain's own read path) ---
        //
        // `code:get_object_code/1` is how the boot chain reads a module off the code path: the
        // module tag, the binary image and the *file name* it was read from. That file name must
        // be the one `code:which/1` resolves, and it must carry the frozen compiled-artifact
        // extension (`code:objfile_extension/0`). Only the tag, the `error` refusal and those
        // *relations* are pinned — never a path value.
        eval(
            "boot/get_object_code_module_is_tagged_by_the_module",
            "code:get_object_code/1 tags its result with the module it read (lists)",
            "element(1, code:get_object_code(lists)) =:= lists",
            "ok true",
        ),
        eval(
            "boot/get_object_code_filename_is_the_code_which_path",
            "the object-file name code:get_object_code/1 reports is the path code:which/1 resolves",
            "element(3, code:get_object_code(lists)) =:= code:which(lists)",
            "ok true",
        ),
        eval(
            "boot/get_object_code_filename_suffix_is_the_objfile_extension",
            "the object-file name ends in the frozen compiled-artifact extension (code:objfile_extension/0)",
            "lists:suffix(code:objfile_extension(), element(3, code:get_object_code(lists)))",
            "ok true",
        ),
        eval(
            "boot/get_object_code_unknown_module_is_error",
            "code:get_object_code/1 reports the atom `error` for a module that exists nowhere",
            "code:get_object_code(no_such_module_xyz)",
            "ok error",
        ),
        eval(
            "boot/get_object_code_preloaded_module_is_tagged",
            "code:get_object_code/1 tags the preloaded `init` module with that module",
            "element(1, code:get_object_code(init)) =:= init",
            "ok true",
        ),
        eval(
            "boot/get_object_code_kernel_module_is_tagged",
            "code:get_object_code/1 tags the code server's own module (`code`) with that module",
            "element(1, code:get_object_code(code)) =:= code",
            "ok true",
        ),
        // --- The code server's lib_dir/2 resolution and code-path resolution floor --------
        //
        // `code:lib_dir/2` resolves an application's subdirectory (the `ebin` the boot chain
        // loads from); the code path must resolve the whole booted stdlib floor. Sub-paths are
        // pinned by their *relation* to `code:lib_dir/1`, never by value.
        eval(
            "boot/code_lib_dir_2_ebin_is_a_string",
            "code:lib_dir/2 resolves an application's subdirectory (stdlib/ebin) to a string",
            "is_list(code:lib_dir(stdlib, ebin))",
            "ok true",
        ),
        eval(
            "boot/code_lib_dir_2_is_under_the_application_dir",
            "an application's subdirectory (stdlib/ebin) extends its application directory",
            "lists:prefix(code:lib_dir(stdlib), code:lib_dir(stdlib, ebin))",
            "ok true",
        ),
        eval(
            "boot/code_lib_dir_2_differs_from_the_application_dir",
            "code:lib_dir/2's subdirectory is not the application directory itself",
            "code:lib_dir(stdlib, ebin) =/= code:lib_dir(stdlib)",
            "ok true",
        ),
        // `code:lib_dir/2`'s `priv` sub-directory and `code:priv_dir/1` are how the boot chain
        // reaches an application's private data; both resolve to a *relation* to the application
        // directory (never a path by value), and an unknown application is refused with the
        // frozen `bad_name` atom.
        eval(
            "boot/code_lib_dir_2_priv_extends_the_application_dir",
            "an application's `priv` subdirectory extends its application directory (code:lib_dir/2)",
            "lists:prefix(code:lib_dir(kernel), code:lib_dir(kernel, priv))",
            "ok true",
        ),
        eval(
            "boot/code_priv_dir_kernel_is_a_string",
            "code:priv_dir/1 resolves kernel's private directory to a string (never a path by value)",
            "is_list(code:priv_dir(kernel))",
            "ok true",
        ),
        eval(
            "boot/code_priv_dir_missing_app_is_bad_name",
            "code:priv_dir/1 reports the frozen {error,bad_name} for an unknown application",
            "code:priv_dir(no_such_application_xyz)",
            "ok {error,bad_name}",
        ),
        // --- The code path's own composition (the boot chain's resolution order) -----------
        //
        // The booted code path is installation-specific, so only its *composition* is pinned:
        // every entry is a string (above), the appended current-directory entry is present and
        // last, and the first entry is an absolute directory. No path is asserted by value.
        eval(
            "boot/code_path_contains_the_current_directory_entry",
            "the booted code path carries the current-directory entry \".\"",
            "lists:member(\".\", code:get_path())",
            "ok true",
        ),
        eval(
            "boot/code_path_last_entry_is_the_current_directory",
            "the current-directory entry is the code path's appended last entry",
            "lists:last(code:get_path()) =:= \".\"",
            "ok true",
        ),
        eval(
            "boot/code_path_head_entry_is_absolute",
            "the code path's first entry is an absolute directory (never asserted by value)",
            "filename:pathtype(hd(code:get_path())) =:= absolute",
            "ok true",
        ),
        eval(
            "boot/code_ensure_loaded_stdlib_floor",
            "the code path resolves every module of the booted stdlib floor to {module,Module}",
            "lists:all(fun(M) -> code:ensure_loaded(M) =:= {module,M} end, [lists,string,ordsets,proplists,maps,queue,gb_trees,calendar,filename,base64,unicode])",
            "ok true",
        ),
        eval(
            "boot/ensure_loaded_preloaded_module_is_module",
            "code:ensure_loaded/1 answers {module,Module} for a preloaded module (erlang)",
            "code:ensure_loaded(erlang)",
            "ok {module,erlang}",
        ),
        eval(
            "boot/ensure_loaded_preloaded_zlib_is_module",
            "code:ensure_loaded/1 answers {module,Module} for a preloaded module (zlib)",
            "code:ensure_loaded(zlib)",
            "ok {module,zlib}",
        ),
        eval(
            "boot/ensure_loaded_makes_module_visible_in_all_loaded",
            "after code:ensure_loaded/1 the module is a member of code:all_loaded/0",
            "begin code:ensure_loaded(lists), lists:keymember(lists, 1, code:all_loaded()) end",
            "ok true",
        ),
        eval(
            "boot/code_is_loaded_matches_which_after_ensure_loaded",
            "after code:ensure_loaded/1, code:is_loaded/1 is the {file,Path} pair of code:which/1",
            "begin code:ensure_loaded(lists), code:is_loaded(lists) =:= {file, code:which(lists)} end",
            "ok true",
        ),
        // --- code:all_loaded/0 invariants the boot chain maintains ------------------------
        eval(
            "boot/all_loaded_entries_are_pairs",
            "every code:all_loaded/0 entry is a two-element {Module,Beam} pair",
            "lists:all(fun(T) -> tuple_size(T) =:= 2 end, code:all_loaded())",
            "ok true",
        ),
        eval(
            "boot/all_loaded_module_names_are_atoms",
            "every module name projected out of code:all_loaded/0 is an atom",
            "lists:all(fun(M) -> is_atom(M) end, [M || {M,_} <- code:all_loaded()])",
            "ok true",
        ),
        eval(
            "boot/all_loaded_covers_the_preloaded_floor",
            "the loaded-module registry is at least as large as the preloaded set",
            "length([M || {M,_} <- code:all_loaded()]) >= length(erlang:pre_loaded())",
            "ok true",
        ),
        // --- The booted system's own process and named-process surface --------------------
        //
        // The kernel processes the boot chain starts, asserted **by shape** (a pid, a list, an
        // integer count) — never by an exact pid, and never by which processes exist.
        eval(
            "boot/group_leader_is_pid",
            "erlang:group_leader/0 is the pid of the boot chain's I/O group leader",
            "is_pid(group_leader())",
            "ok true",
        ),
        eval(
            "boot/self_is_alive",
            "the running process is alive according to erlang:is_process_alive/1",
            "erlang:is_process_alive(self())",
            "ok true",
        ),
        eval(
            "boot/processes_members_are_pids",
            "every member of erlang:processes/0 is a pid",
            "lists:all(fun erlang:is_pid/1, erlang:processes())",
            "ok true",
        ),
        eval(
            "boot/process_count_matches_the_process_list",
            "erlang:system_info(process_count) equals the length of erlang:processes/0",
            "erlang:system_info(process_count) =:= length(erlang:processes())",
            "ok true",
        ),
        eval(
            "boot/registered_is_a_list",
            "erlang:registered/0 is the booted system's registered-name table (a list)",
            "is_list(erlang:registered())",
            "ok true",
        ),
        Case {
            id: "boot/register_of_a_boot_name_is_badarg",
            description: "the boot chain's own `init` name cannot be re-registered (register/2 raises badarg)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin P = spawn(fun() -> receive _ -> ok end end), register(init, P) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        eval(
            "boot/init_get_status_size_is_two",
            "init:get_status/0 is the two-element {State,Phase} boot-status tuple",
            "tuple_size(init:get_status()) =:= 2",
            "ok true",
        ),
        eval(
            "boot/process_info_memory_shape",
            "erlang:process_info/2 reports the process's memory as the {memory,Words} pair",
            "begin M = process_info(self(), memory), is_tuple(M) andalso element(1, M) =:= memory andalso is_integer(element(2, M)) end",
            "ok true",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Contract, Probe};

    /// The process cases this group adds whose *spawn* is expected to succeed today: they are
    /// asserted **by shape** (a pid, a list, a tag), never by an exact pid/ref/port value.
    const SHAPE_CASES: &[&str] = &[
        "boot/spawn_returns_pid",
        "boot/spawn_link_returns_pid",
        "boot/spawn_opt_returns_pid",
        "boot/spawn_opt_empty_options_returns_pid",
        "boot/spawned_process_round_trip",
        "boot/receive_after_timeout",
        "boot/receive_after_message_arrives",
        "boot/processes_is_list",
        "boot/processes_contains_self",
    ];

    /// The process-dictionary cases: pinned against the oracle's own golden, asserted by
    /// value (a small integer or the atom `undefined`).
    const PROCESS_DICTIONARY_CASES: &[&str] = &[
        "boot/process_dictionary_put_get",
        "boot/process_dictionary_get_missing",
        "boot/process_dictionary_erase",
    ];

    fn case<'a>(cases: &'a [super::Case], id: &str) -> &'a super::Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the boot group"))
    }

    #[test]
    fn process_cases_are_registered_as_build_eval_cases() {
        let cases = group();
        let cases = cases.as_slice();
        for id in SHAPE_CASES.iter().chain(PROCESS_DICTIONARY_CASES) {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::Build], "case {id}");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id} must be an Eval case"
            );
            assert!(
                case.extra_argv().is_empty(),
                "case {id} takes no extra argv"
            );
        }
    }

    #[test]
    fn process_cases_never_pin_an_unstable_pid() {
        let cases = group();
        let cases = cases.as_slice();
        for id in SHAPE_CASES.iter().chain(PROCESS_DICTIONARY_CASES) {
            let golden = case(cases, id)
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {id} asserts stdout"));
            assert!(
                !golden.contains("<0."),
                "case {id}: the golden pins an unstable pid: {golden:?}"
            );
        }
    }

    #[test]
    fn spawn_cases_assert_pid_shape_not_identity() {
        let cases = group();
        let cases = cases.as_slice();
        for id in [
            "boot/spawn_returns_pid",
            "boot/spawn_link_returns_pid",
            "boot/spawn_opt_returns_pid",
            "boot/spawn_opt_empty_options_returns_pid",
        ] {
            let CaseKind::Eval { expr, probe, .. } = case(cases, id).kind else {
                panic!("case {id} must be an Eval case");
            };
            assert_eq!(probe, Probe::TermToBinary, "case {id}");
            assert!(expr.starts_with("is_pid("), "case {id}: {expr:?}");
        }
    }

    #[test]
    fn receive_after_case_uses_a_non_zero_timeout() {
        let cases = group();
        let cases = cases.as_slice();
        let CaseKind::Eval { expr, .. } = case(cases, "boot/receive_after_timeout").kind else {
            panic!("must be an Eval case");
        };
        assert!(
            expr.contains("after 50 ->"),
            "the receive/after case must use a non-zero timeout: {expr:?}"
        );
        assert!(
            expr.contains("timeout"),
            "the receive/after case must assert the timeout outcome: {expr:?}"
        );
    }

    #[test]
    fn process_dictionary_cases_exercise_put_get_erase() {
        let cases = group();
        let cases = cases.as_slice();
        let exprs: Vec<&str> = PROCESS_DICTIONARY_CASES
            .iter()
            .map(|id| case(cases, id).expr().expect("an Eval case has an expr"))
            .collect();
        assert!(exprs.iter().any(|e| e.contains("put(")), "no put/2 case");
        assert!(exprs.iter().any(|e| e.contains("get(")), "no get case");
        assert!(exprs.iter().any(|e| e.contains("erase(")), "no erase/1 case");
    }

    /// The **load-surface** cases this group adds on top of the code-server/path cases: the
    /// object-file read path (`code:get_object_code/1`), `code:lib_dir/2`, the code path's
    /// resolution of the booted stdlib floor, the `code:all_loaded/0` invariants, and the
    /// booted system's own process/named-process surface. Every assertion is a **relation**, a
    /// **shape**, or a frozen value; a path, a pid and a count are never pinned by value.
    const LOAD_SURFACE_CASES: &[&str] = &[
        "boot/get_object_code_module_is_tagged_by_the_module",
        "boot/get_object_code_filename_is_the_code_which_path",
        "boot/get_object_code_filename_suffix_is_the_objfile_extension",
        "boot/get_object_code_unknown_module_is_error",
        "boot/get_object_code_preloaded_module_is_tagged",
        "boot/get_object_code_kernel_module_is_tagged",
        "boot/code_lib_dir_2_ebin_is_a_string",
        "boot/code_lib_dir_2_is_under_the_application_dir",
        "boot/code_lib_dir_2_differs_from_the_application_dir",
        "boot/code_lib_dir_2_priv_extends_the_application_dir",
        "boot/code_priv_dir_kernel_is_a_string",
        "boot/code_priv_dir_missing_app_is_bad_name",
        "boot/code_path_contains_the_current_directory_entry",
        "boot/code_path_last_entry_is_the_current_directory",
        "boot/code_path_head_entry_is_absolute",
        "boot/code_ensure_loaded_stdlib_floor",
        "boot/ensure_loaded_preloaded_module_is_module",
        "boot/ensure_loaded_preloaded_zlib_is_module",
        "boot/ensure_loaded_makes_module_visible_in_all_loaded",
        "boot/code_is_loaded_matches_which_after_ensure_loaded",
        "boot/all_loaded_entries_are_pairs",
        "boot/all_loaded_module_names_are_atoms",
        "boot/all_loaded_covers_the_preloaded_floor",
        "boot/group_leader_is_pid",
        "boot/self_is_alive",
        "boot/processes_members_are_pids",
        "boot/process_count_matches_the_process_list",
        "boot/registered_is_a_list",
        "boot/init_get_status_size_is_two",
        "boot/process_info_memory_shape",
    ];

    /// The one negative load-surface case: a name the boot chain owns cannot be re-registered.
    const LOAD_SURFACE_NEGATIVE_CASES: &[&str] = &["boot/register_of_a_boot_name_is_badarg"];

    #[test]
    fn load_surface_cases_are_build_eval_cases_with_reviewed_goldens() {
        let cases = group();
        let cases = cases.as_slice();
        for id in LOAD_SURFACE_CASES.iter().chain(LOAD_SURFACE_NEGATIVE_CASES) {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::Build], "case {id}");
            assert_eq!(case.kind_label(), "value", "case {id}");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id} must be an Eval case"
            );
            assert!(
                case.extra_argv().is_empty(),
                "case {id} takes no extra argv"
            );
            assert!(case.env().is_empty(), "case {id} pins no environment");
            assert!(
                !case.description.is_empty(),
                "case {id} has no description"
            );
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {id} asserts stdout"));
            assert!(
                LOAD_SURFACE_GOLDENS.contains(&golden),
                "case {id}: golden {golden:?} is outside the reviewed set"
            );
            // No volatile rendering may be baked into a golden: no pid/ref/port (`<`), no
            // filesystem path (`/`), no map (`#{`).
            for unstable in ["<", "/", "#{"] {
                assert!(
                    !golden.contains(unstable),
                    "case {id}: the golden pins an unstable rendering: {golden:?}"
                );
            }
        }
        assert_eq!(
            case(cases, LOAD_SURFACE_NEGATIVE_CASES[0]).golden.exit_code,
            1,
            "the negative case exits non-zero"
        );
    }

    /// Every golden the load-surface cases may assert. Explicit (rather than a loose shape rule)
    /// so a new case cannot slip a volatile rendering past review.
    const LOAD_SURFACE_GOLDENS: &[&str] = &[
        "ok true",
        "ok error",
        "ok {module,erlang}",
        "ok {module,zlib}",
        "ok {error,bad_name}",
        "exception error badarg",
    ];

    /// No load-surface expression may bind the probe wrapper's own names (`V`, `C`, `R`): the
    /// *oracle* would reject the wrapped probe (`unsafe_var`), a guaranteed false mismatch.
    #[test]
    fn load_surface_expressions_do_not_shadow_the_probe_variables() {
        let cases = group();
        let cases = cases.as_slice();
        for id in LOAD_SURFACE_CASES.iter().chain(LOAD_SURFACE_NEGATIVE_CASES) {
            let expr = case(cases, id).expr().expect("an Eval case has an expr");
            for reserved in [" V ", " C ", " R ", "V =", "C =", "R ="] {
                assert!(
                    !expr.contains(reserved),
                    "case {id}: the expression binds the probe variable {reserved:?}: {expr:?}"
                );
            }
        }
    }
}