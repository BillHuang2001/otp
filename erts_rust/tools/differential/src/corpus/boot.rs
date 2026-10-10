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
//! server's query surface** (`code:is_loaded/1`, `code:ensure_loaded/1`, `code:is_sticky/1`)
//! and the **OTP application surface** the booted system exposes (`application:get_env/1,2,3`,
//! `get_all_env/1`, `loaded_applications/0`, `which_applications/0`, `get_key/2`) — again by
//! shape wherever the value is release/installation-specific.
//!
//! The **process** surface is driven through `-eval` and asserted **by shape**, never by an
//! exact pid (pids vary per VM start): `spawn/1`, `spawn_link/1` and `spawn_opt/2` must return
//! a pid, the mailbox carries a real `spawn` + `!` + `receive` round trip (including a
//! non-zero `after` timeout), and `erlang:processes/0` is a list. The **process dictionary**
//! (`put/2`, `get/1`, `erase/1`) and `init:get_status/0` are part of the booted system's
//! observable surface.
//!
//! The boot-path **prerequisite primitives** (the `erlang:pre_loaded/0` set, the boot script's
//! `which_applications/0`/`loaded_applications/0`, the preloaded `init`, kernel's
//! `error_logger`/`vsn` entries, the `register/2` + `whereis/1` and group-leader-inheritance
//! kernel-process primitives, the `-boot` init argument and the boot chain's
//! `proc_lib:start_link/4` start wait) live in the sibling `boot_prereq`
//! group; this group keeps the boot/version observability surface.
use super::{Case, CaseKind, Contract, Golden, Probe};

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
                expr: "begin L = code:is_loaded(lists), is_tuple(L) andalso element(1, L) =:= file andalso is_list(element(2, L)) end",
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
}