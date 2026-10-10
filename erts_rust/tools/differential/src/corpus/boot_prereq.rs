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

//! Contract **A.9** — the boot-path **prerequisite primitives**.
//!
//! These are the surfaces the *real* `bootstrap/bin/start.boot` chain relies on the booted
//! system answering: the `erlang:pre_loaded/0` set, the applications the boot script has
//! started (`application:which_applications/0`, `application:loaded_applications/0`), the
//! code server's view of the preloaded `init`, kernel's `error_logger`/`vsn` application
//! entries, the boot script's own `register/2` + `whereis/1` and group-leader-inheritance
//! kernel-process primitives, and the `-boot` init argument (contract A.6) with the
//! **stopping point** of the real boot chain (`proc_lib:start_link/4`'s cross-module
//! `proc_lib:sync_start/2` wait, folded into the engine's typed `blocked` gap ⇒ the one honest
//! `unsupported` case here). Every other case below `agree`s.
//!
//! The group also pins the **booted system's own self-description** — the state a drop-in
//! runtime must reproduce to be a plausible boot target: `init`'s recorded arguments
//! (`root`/`home`/`noshell`/`-pa`, and the absence of any ungiven flag) and boot status, the
//! code server's resolution of every OTP application's library directory and of the *preloaded*
//! module set (`code:which/1` ⇒ `preloaded`, `code:is_loaded/1` ⇒ `{file,preloaded}`,
//! `erlang:module_loaded/1` ⇒ `true`), the OTP application metadata the boot script leaves
//! behind (`application:get_key/2`, `application:get_env/1,2`, `application:get_application/0`),
//! and the emulator's version/flavour constants. Wherever a value is machine- or
//! release-dependent (a filesystem path, the `otp_release` string, a scheduler count) the case
//! asserts the **shape**; only release-stable constants are pinned by value. No case pins a
//! pid/ref/creation or a multi-key-map `~w` rendering.
//!
//! The case id's `<group>/` segment is a **subdivision, not the Rust file name** (see
//! `super`'s module docs), so these `boot/…` ids living in `boot_prereq.rs` is expected: this
//! group is the boot-path *prerequisite* half of the A.9 surface, while `boot` (in `boot.rs`)
//! holds the boot/version-observability half and `boot_extra` the preloaded-module invariants.
//!
//! The surfaces deliberately **not** pinned here are plain differing observables — genuine
//! divergences that would be a `mismatch`, not a capability gap — and are handoffs to `beam_rt`
//! (the oracle value state, verified with `differential compare` against the live system ERTS,
//! is what the runtime should converge on):
//!
//! | surface | oracle (OTP 29 / erts-17.1) | rust runtime |
//! |---|---|---|
//! | `init:get_argument(progname)` | `{ok,[["erl"]]}` | `{ok,[["beam_rt"]]}` (the runtime's own invocation name — correct in kind, differing in value) |
//! | `init:script_id/0` | `{"Erlang/OTP","29"}` | raises `badarg` |
//! | `init:get_arguments/0` | every flag **plus** the emulator's `root`/`bindir`/`progname`/`home` | the runtime's own modelled flags only |
//! | `erlang:module_loaded(lists)` (a non-preloaded module) | `true` | `false` |
//! | `code:lib_dir/2` (e.g. `code:lib_dir(kernel, ebin)`) | a path string | raises `badarg` |
//! | `code:get_path/0` | the installation's path list | a differing path list |
//! | `application:get_all_key/1`, `application:get_key/1` | `{ok,KeyValueList}` / `{ok,KeyValueList}` | raises `badarg` |
//! | `application:get_key(App, mod)` on a library app (e.g. `stdlib`) | `{ok,[]}` | `undefined` |
//! | `application:get_key(kernel, env\|maxT\|runtime_dependencies\|…)` | the key's value | `undefined` |
//! | `erlang:system_info(loaded)` | the loaded-code table | a typed `unsupported` capability gap (not a mismatch) |
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

/// One A.9 boot-observability case that also passes **extra CLI argv** to both runtimes
/// before `-eval` (the `-pa` code-path switch, contract A.6).
fn eval_with_argv(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    extra_argv: &'static [&'static str],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::Build],
        kind: CaseKind::Eval {
            expr,
            extra_argv,
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.9 boot-path prerequisite cases, asserted by shape or by a release-stable value.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- Boot-path prerequisite primitives -------------------------------------------
        //
        // The primitives the *real* `bootstrap/bin/start.boot` chain relies on now that the
        // booted system answers them: the preloaded-module set, the applications the boot
        // script has loaded, the code server's view of the preloaded `init`, and the
        // init/boot status. The **named-process** surface (`register/2` + `whereis/1`) and
        // the **group-leader** inheritance rule are the boot script's own kernel-process
        // primitives — a booted system could not start `application_controller` without them.
        Case {
            id: "boot/pre_loaded_length_is_22",
            description: "erlang:pre_loaded/0 is the boot-preloaded module set (22 modules)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "length(erlang:pre_loaded())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 22"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/pre_loaded_exact_list",
            description: "erlang:pre_loaded/0 reproduces the frozen 22-atom preloaded set exactly",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:sort(erlang:pre_loaded()) =:= [atomics,counters,erl_init,erl_prim_loader,erl_tracer,erlang,erts_code_purger,erts_dirty_process_signal_handler,erts_internal,erts_literal_area_collector,erts_trace_cleaner,init,persistent_term,prim_buffer,prim_eval,prim_file,prim_inet,prim_net,prim_socket,prim_zip,socket_registry,zlib]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/which_applications_has_kernel",
            description: "the boot script has started `kernel` (it appears in which_applications/0)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "lists:keymember(kernel, 1, application:which_applications())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/which_applications_is_non_empty",
            description: "application:which_applications/0 is non-empty once the boot script has run",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "length(application:which_applications()) > 0",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/loaded_applications_at_least_two",
            description: "the boot script has loaded at least the `kernel` and `stdlib` applications",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "length([A || {A, _, _} <- application:loaded_applications()]) >= 2",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_is_loaded_init_is_preloaded",
            description: "the booted `init` module is a preloaded module (code:is_loaded/1 -> {file,preloaded})",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "code:is_loaded(init)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {file,preloaded}"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/init_get_status_reports_started",
            description: "init:get_status/0 reports the started state {starting,started} once booted",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "init:get_status()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {starting,started}"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_env_kernel_error_logger_undefined",
            description: "kernel's `error_logger` env key is unset by the boot script (application:get_env/2 -> undefined)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "application:get_env(kernel, error_logger)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok undefined"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/application_get_key_kernel_vsn_is_ok",
            description: "application:get_key/2 for kernel's `vsn` is tagged `ok` (shape, not the release version)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "element(1, application:get_key(kernel, vsn))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ok"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/register_and_whereis_round_trip",
            description: "a boot-script-style named process: register/2 then whereis/1 resolves to the same pid",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin P = spawn(fun() -> receive _ -> ok end end), register(boot_named_probe, P), whereis(boot_named_probe) =:= P end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/spawned_process_inherits_group_leader",
            description: "a spawned process inherits its parent's group leader (the boot script's kernel-process link)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin Self = self(), GL = erlang:group_leader(), P = spawn(fun() -> receive {q, From} -> From ! erlang:group_leader() end end), P ! {q, Self}, receive ChildGL -> is_pid(ChildGL) andalso ChildGL =:= GL end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- The `-boot` init argument and the boot chain's blocked seam -----------------
        //
        // The black-box CLI records `-boot FILE` as the init argument `boot` (contract A.6)
        // but the Rust runtime does not *execute* the boot script, so the real
        // `bootstrap/bin/start.boot` chain is stopped. `-boot start` is the value both
        // runtimes resolve identically (the oracle appends `.boot`, so the bare `start`, not
        // `start.boot`, is what agrees). The negative shape pins the argument's absence.
        Case {
            id: "boot/boot_flag_is_recorded_as_init_argument",
            description: "`-boot start` is recorded as the init argument `boot` (a booted init surface)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "init:get_argument(boot) =:= {ok, [[\"start\"]]}",
                extra_argv: &["-boot", "start"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/no_boot_flag_is_badarg",
            description: "with no `-boot`, init:get_argument/1 has no `boot` argument (element(1,_) raises badarg)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "element(1, init:get_argument(boot))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // The **stopping point** of the real boot chain, pinned as a typed capability gap.
        // The boot path's `{kernelProcess, application_controller, {application_controller,start}}`
        // directive is reached through `proc_lib:start_link/4`, whose cross-module
        // `proc_lib:sync_start/2` `receive` the engine folds into `blocked()`: a boot-chain
        // child that never acks suspends the process, so the runtime signals the engine's
        // unchanged `blocked` diagnostic on exit 3 (never a wrong result, never a hang) while
        // the live oracle answers the same shape. `erlang:node/0` is a child that returns
        // normally, so the oracle produces a clean, stable line (a crashing child would emit
        // a timestamped crash report and break the golden). NOT corpus/pinned: the sibling
        // boot-path surfaces `application:load/1` / `application:start/1` /
        // `whereis(application_controller)` / `init:get_arguments/0` / `code:priv_dir/1` /
        // `application:get_all_key/1`, which currently diverge from the oracle with a *plain*
        // Erlang reason (`exit noproc` / `undefined` / `badarg`) and would (correctly) be a
        // `mismatch`.
        Case {
            id: "boot/proc_lib_start_link_blocks_the_boot_path",
            description: "the boot chain's proc_lib:start_link/4 wait is a cross-module receive the engine folds into a typed `blocked` capability gap",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "begin catch proc_lib:start_link(erlang, node, []), ok end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,111,107>>"),
                exit_code: 0,
            },
        },
        // === The booted system's self-description ========================================
        //
        // What the *booted* runtime reports about itself once the boot script has run — the
        // `init` arguments it recorded, the code server's resolution of the preloaded floor
        // and of every OTP application, the OTP application metadata the boot chain leaves
        // behind, and the emulator's version/flavour constants. A drop-in boot target must
        // reproduce all of them to a plausible degree; each case below pins the **shape** or
        // a **release-stable value**, never a machine path or a per-VM identity.
        //
        // --- `init`: the recorded boot arguments and the boot status ----------------------
        eval(
            "boot/init_get_argument_root_is_a_non_empty_string",
            "init:get_argument(root) is {ok,[[Path]]} with the installation root the system booted from (path never pinned by value)",
            "begin {ok, [[RootPath]]} = init:get_argument(root), is_list(RootPath) andalso length(RootPath) > 0 end",
            "ok true",
        ),
        eval(
            "boot/init_get_argument_home_shape",
            "init:get_argument(home) is `error` or {ok,[[Path]]} (both runtimes read the same $HOME)",
            "begin Home = init:get_argument(home), Home =:= error orelse (is_tuple(Home) andalso is_list(element(2, Home))) end",
            "ok true",
        ),
        eval(
            "boot/init_get_plain_arguments_is_empty",
            "init:get_plain_arguments/0 is the empty list for the corpus argv (no -extra arguments)",
            "init:get_plain_arguments() =:= []",
            "ok true",
        ),
        eval(
            "boot/init_get_argument_noshell_is_ok_empty",
            "the -noshell flag the harness always passes is exactly {ok,[[]]} (a valueless flag)",
            "init:get_argument(noshell) =:= {ok, [[]]}",
            "ok true",
        ),
        eval(
            "boot/init_get_argument_absent_flag_is_error",
            "init:get_argument/1 reports the atom `error` for a flag that was never passed",
            "init:get_argument(no_such_flag_xyz)",
            "ok error",
        ),
        eval(
            "boot/init_get_status_first_element_is_starting",
            "init:get_status/0's first element is the `starting` boot phase",
            "element(1, init:get_status()) =:= starting",
            "ok true",
        ),
        eval(
            "boot/init_get_status_second_element_is_started",
            "init:get_status/0's second element is the `started` boot phase",
            "element(2, init:get_status()) =:= started",
            "ok true",
        ),
        eval_with_argv(
            "boot/init_get_argument_pa_records_the_added_path",
            "a `-pa <dir>` code-path switch is recorded as the init argument `pa` with that exact value",
            "init:get_argument(pa) =:= {ok, [[\"/nonexistent/marker\"]]}",
            &["-pa", "/nonexistent/marker"],
            "ok true",
        ),
        // --- The code server: mode, library directories, preloaded resolution -------------
        eval(
            "boot/code_get_mode_is_interactive",
            "code:get_mode/0 reports `interactive` for a command line without -mode embedded",
            "code:get_mode() =:= interactive",
            "ok true",
        ),
        eval(
            "boot/code_lib_dir_compiler_is_a_non_empty_string",
            "code:lib_dir/1 resolves the `compiler` application to a non-empty directory (path never pinned by value)",
            "begin D = code:lib_dir(compiler), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot/code_lib_dir_erts_is_a_non_empty_string",
            "code:lib_dir/1 resolves the `erts` application to a non-empty directory (path never pinned by value)",
            "begin D = code:lib_dir(erts), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot/code_lib_dir_sasl_is_a_non_empty_string",
            "code:lib_dir/1 resolves the `sasl` application to a non-empty directory (path never pinned by value)",
            "begin D = code:lib_dir(sasl), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot/code_lib_dir_many_applications_resolve",
            "code:lib_dir/1 resolves a broad set of boot-installed OTP applications to non-empty directories",
            "lists:all(fun(A) -> is_list(code:lib_dir(A)) andalso length(code:lib_dir(A)) > 0 end, [erts,kernel,stdlib,compiler,sasl,crypto,ssl,public_key,syntax_tools,runtime_tools,parsetools,xmerl])",
            "ok true",
        ),
        eval(
            "boot/code_lib_dir_of_distinct_applications_differs",
            "code:lib_dir/1 gives distinct directories to distinct applications (kernel =/= stdlib)",
            "code:lib_dir(kernel) =/= code:lib_dir(stdlib)",
            "ok true",
        ),
        eval(
            "boot/code_which_erts_internal_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the preloaded `erts_internal` module",
            "code:which(erts_internal)",
            "ok preloaded",
        ),
        eval(
            "boot/code_which_prim_zip_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the preloaded `prim_zip` module",
            "code:which(prim_zip)",
            "ok preloaded",
        ),
        eval(
            "boot/code_which_socket_registry_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the preloaded `socket_registry` module",
            "code:which(socket_registry)",
            "ok preloaded",
        ),
        eval(
            "boot/code_which_erl_prim_loader_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the boot loader `erl_prim_loader`",
            "code:which(erl_prim_loader)",
            "ok preloaded",
        ),
        eval(
            "boot/code_is_loaded_prim_net_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for the preloaded `prim_net` module",
            "code:is_loaded(prim_net)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot/code_is_loaded_socket_registry_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for the preloaded `socket_registry` module",
            "code:is_loaded(socket_registry)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot/code_is_loaded_erts_code_purger_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for the preloaded `erts_code_purger` module",
            "code:is_loaded(erts_code_purger)",
            "ok {file,preloaded}",
        ),
        // --- The preloaded floor through `erlang:module_loaded/1` -------------------------
        // The preloaded set is *loaded* in both runtimes. (`module_loaded/1` on a
        // non-preloaded module the runtime has not loaded — e.g. `lists` — is a documented
        // divergence, so only the preloaded members are pinned here.)
        eval(
            "boot/module_loaded_every_preloaded_module",
            "erlang:module_loaded/1 reports every member of erlang:pre_loaded/0 as loaded",
            "lists:all(fun(M) -> erlang:module_loaded(M) end, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot/module_loaded_preloaded_count_is_22",
            "exactly the 22 preloaded modules answer true to erlang:module_loaded/1",
            "length([M || M <- erlang:pre_loaded(), erlang:module_loaded(M)]) =:= 22",
            "ok true",
        ),
        eval(
            "boot/module_loaded_erlang_is_true",
            "erlang:module_loaded/1 reports the preloaded `erlang` module as loaded",
            "erlang:module_loaded(erlang)",
            "ok true",
        ),
        eval(
            "boot/module_loaded_init_is_true",
            "erlang:module_loaded/1 reports the preloaded `init` module as loaded",
            "erlang:module_loaded(init)",
            "ok true",
        ),
        eval(
            "boot/module_loaded_erts_internal_is_true",
            "erlang:module_loaded/1 reports the preloaded `erts_internal` module as loaded",
            "erlang:module_loaded(erts_internal)",
            "ok true",
        ),
        eval(
            "boot/module_loaded_prim_net_is_true",
            "erlang:module_loaded/1 reports the preloaded `prim_net` module as loaded",
            "erlang:module_loaded(prim_net)",
            "ok true",
        ),
        eval(
            "boot/module_loaded_erl_prim_loader_is_true",
            "erlang:module_loaded/1 reports the preloaded boot loader `erl_prim_loader` as loaded",
            "erlang:module_loaded(erl_prim_loader)",
            "ok true",
        ),
        // --- The OTP application metadata the boot script leaves behind -------------------
        eval(
            "boot/application_get_key_kernel_applications_shape",
            "application:get_key(kernel, applications) is {ok,List} (the application-dependency key)",
            "begin K = application:get_key(kernel, applications), is_tuple(K) andalso element(1, K) =:= ok andalso is_list(element(2, K)) end",
            "ok true",
        ),
        eval(
            "boot/application_get_key_kernel_modules_is_a_list",
            "application:get_key(kernel, modules) carries the application's module list",
            "is_list(element(2, application:get_key(kernel, modules)))",
            "ok true",
        ),
        eval(
            "boot/application_get_key_stdlib_modules_is_a_list",
            "application:get_key(stdlib, modules) carries the application's module list",
            "is_list(element(2, application:get_key(stdlib, modules)))",
            "ok true",
        ),
        eval(
            "boot/application_get_key_stdlib_vsn_is_ok",
            "application:get_key/2 for stdlib's `vsn` is tagged `ok` (shape, not the release version)",
            "element(1, application:get_key(stdlib, vsn)) =:= ok",
            "ok true",
        ),
        eval(
            "boot/application_get_application_is_undefined_in_eval",
            "application:get_application/0 is `undefined` for a process started outside any application",
            "application:get_application()",
            "ok undefined",
        ),
        eval(
            "boot/application_get_env_stdlib_shape",
            "application:get_env/1 for stdlib is `undefined` or {ok,Value} (shape, not the machine value)",
            "begin Env = application:get_env(stdlib), Env =:= undefined orelse (is_tuple(Env) andalso element(1, Env) =:= ok) end",
            "ok true",
        ),
        eval(
            "boot/application_get_env_stdlib_missing_key_is_undefined",
            "application:get_env/2 reports `undefined` for an unset stdlib key",
            "application:get_env(stdlib, no_such_key_xyz) =:= undefined",
            "ok true",
        ),
        // --- The emulator's version / flavour constants -----------------------------------
        eval(
            "boot/system_info_nif_and_driver_version_are_strings",
            "erlang:system_info(nif_version) and (driver_version) are strings (values are release-dependent)",
            "is_list(erlang:system_info(nif_version)) andalso is_list(erlang:system_info(driver_version))",
            "ok true",
        ),
        eval(
            "boot/system_info_emu_flavor_and_machine_shape",
            "erlang:system_info(emu_flavor) is an atom and (machine) is the frozen \"BEAM\" string",
            "is_atom(erlang:system_info(emu_flavor)) andalso is_list(erlang:system_info(machine))",
            "ok true",
        ),
        eval(
            "boot/system_info_otp_release_and_version_shape",
            "erlang:system_info(otp_release) and (version) are strings (otp_release is release-dependent, never pinned by value)",
            "is_list(erlang:system_info(otp_release)) andalso is_list(erlang:system_info(version))",
            "ok true",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Contract, Probe};

    /// The **boot-path prerequisite primitive** cases this group adds: plain `-eval` cases
    /// (no extra argv) whose oracle golden the Rust runtime reproduces, asserted by shape or
    /// by a release-stable value. The negative `no_boot_flag` shape is included: its golden
    /// is the oracle's own `exception error badarg` line (exit 1).
    const BOOT_PREREQUISITE_CASES: &[&str] = &[
        "boot/pre_loaded_length_is_22",
        "boot/pre_loaded_exact_list",
        "boot/which_applications_has_kernel",
        "boot/which_applications_is_non_empty",
        "boot/loaded_applications_at_least_two",
        "boot/code_is_loaded_init_is_preloaded",
        "boot/init_get_status_reports_started",
        "boot/application_get_env_kernel_error_logger_undefined",
        "boot/application_get_key_kernel_vsn_is_ok",
        "boot/register_and_whereis_round_trip",
        "boot/spawned_process_inherits_group_leader",
        "boot/no_boot_flag_is_badarg",
    ];

    /// The one case here that takes **extra argv**: the `-boot start` CLI flag (contract A.6).
    const BOOT_FLAG_CASES: &[&str] = &["boot/boot_flag_is_recorded_as_init_argument"];

    /// The one honest **capability gap** this group adds: the boot chain's `proc_lib:start_link/4`
    /// wait, which the engine folds into a typed `blocked` diagnostic (exit 3) — classified
    /// `unsupported`, never a `mismatch`.
    const BOOT_GAP_CASES: &[&str] = &["boot/proc_lib_start_link_blocks_the_boot_path"];

    /// The **booted-system self-description** cases this group adds: plain `-eval` cases (no
    /// extra argv) whose oracle golden the Rust runtime reproduces, asserted by shape or by a
    /// release-stable value. Collected in one list so a new case cannot silently escape review.
    const BOOT_OBSERVABILITY_CASES: &[&str] = &[
        "boot/init_get_argument_root_is_a_non_empty_string",
        "boot/init_get_argument_home_shape",
        "boot/init_get_plain_arguments_is_empty",
        "boot/init_get_argument_noshell_is_ok_empty",
        "boot/init_get_argument_absent_flag_is_error",
        "boot/init_get_status_first_element_is_starting",
        "boot/init_get_status_second_element_is_started",
        "boot/code_get_mode_is_interactive",
        "boot/code_lib_dir_compiler_is_a_non_empty_string",
        "boot/code_lib_dir_erts_is_a_non_empty_string",
        "boot/code_lib_dir_sasl_is_a_non_empty_string",
        "boot/code_lib_dir_many_applications_resolve",
        "boot/code_lib_dir_of_distinct_applications_differs",
        "boot/code_which_erts_internal_is_preloaded",
        "boot/code_which_prim_zip_is_preloaded",
        "boot/code_which_socket_registry_is_preloaded",
        "boot/code_which_erl_prim_loader_is_preloaded",
        "boot/code_is_loaded_prim_net_is_preloaded",
        "boot/code_is_loaded_socket_registry_is_preloaded",
        "boot/code_is_loaded_erts_code_purger_is_preloaded",
        "boot/module_loaded_every_preloaded_module",
        "boot/module_loaded_preloaded_count_is_22",
        "boot/module_loaded_erlang_is_true",
        "boot/module_loaded_init_is_true",
        "boot/module_loaded_erts_internal_is_true",
        "boot/module_loaded_prim_net_is_true",
        "boot/module_loaded_erl_prim_loader_is_true",
        "boot/application_get_key_kernel_applications_shape",
        "boot/application_get_key_kernel_modules_is_a_list",
        "boot/application_get_key_stdlib_modules_is_a_list",
        "boot/application_get_key_stdlib_vsn_is_ok",
        "boot/application_get_application_is_undefined_in_eval",
        "boot/application_get_env_stdlib_shape",
        "boot/application_get_env_stdlib_missing_key_is_undefined",
        "boot/system_info_nif_and_driver_version_are_strings",
        "boot/system_info_emu_flavor_and_machine_shape",
        "boot/system_info_otp_release_and_version_shape",
    ];

    /// The one **new** case that takes extra argv: the `-pa` code-path switch (contract A.6).
    const BOOT_PA_CASES: &[&str] = &["boot/init_get_argument_pa_records_the_added_path"];

    fn case<'a>(cases: &'a [super::Case], id: &str) -> &'a super::Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the boot-prereq group"))
    }

    #[test]
    fn boot_prerequisite_cases_are_registered_as_build_eval_cases() {
        let cases = group();
        let cases = cases.as_slice();
        for id in BOOT_PREREQUISITE_CASES {
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
    fn boot_prerequisite_cases_never_pin_a_volatile_value() {
        let cases = group();
        let cases = cases.as_slice();
        for id in BOOT_PREREQUISITE_CASES.iter().chain(BOOT_FLAG_CASES) {
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
    fn boot_flag_case_carries_the_boot_argument() {
        let cases = group();
        let cases = cases.as_slice();
        let case = case(cases, BOOT_FLAG_CASES[0]);
        assert_eq!(case.contracts, &[Contract::Build], "the -boot case");
        assert_eq!(
            case.extra_argv(),
            &["-boot", "start"],
            "the -boot case must pass `-boot start` (the oracle appends `.boot`)"
        );
        let CaseKind::Eval { expr, probe, .. } = case.kind else {
            panic!("the -boot case must be an Eval case");
        };
        assert_eq!(probe, Probe::Value, "the -boot case");
        assert!(
            expr.contains("init:get_argument(boot)"),
            "the -boot case must observe the `boot` init argument: {expr:?}"
        );
    }

    #[test]
    fn boot_gap_case_is_a_typed_capability_observation() {
        let cases = group();
        let cases = cases.as_slice();
        let case = case(cases, BOOT_GAP_CASES[0]);
        assert_eq!(case.contracts, &[Contract::Build], "the boot-gap case");
        assert!(
            case.extra_argv().is_empty(),
            "the boot-gap case takes no extra argv"
        );
        let CaseKind::Eval { expr, probe, .. } = case.kind else {
            panic!("the boot-gap case must be an Eval case");
        };
        assert_eq!(probe, Probe::TermToBinary, "the boot-gap case");
        assert!(
            expr.contains("proc_lib:start_link"),
            "the boot-gap case must reach the boot chain's start wait: {expr:?}"
        );
        assert!(
            expr.contains("catch"),
            "the boot-gap case must catch the boot chain's wait: {expr:?}"
        );
        // The golden is the *oracle's* clean `ok` line (never a capability-gap marker), so the
        // Rust side is the one that reports the typed gap.
        assert_eq!(case.golden.stdout, Some("ok <<131,119,2,111,107>>"));
        assert_eq!(case.golden.exit_code, 0);
    }

    /// The goldens the new booted-system self-description cases may assert. The list is
    /// explicit (rather than a loose rule) so a new case cannot slip a volatile rendering past
    /// review, and the test also asserts that no entry here is unused. Volatile values (the
    /// installation paths, the `otp_release` string) are observed by shape only, so whatever a
    /// case asserts is a frozen constant or a boolean/tuple-shaped predicate.
    const NEW_GOLDENS: &[&str] = &[
        "ok true",
        "ok error",
        "ok preloaded",
        "ok {file,preloaded}",
        "ok undefined",
    ];

    #[test]
    fn boot_observability_cases_are_build_eval_cases_with_stable_goldens() {
        use std::collections::HashSet;
        let cases = group();
        let cases = cases.as_slice();
        let mut used: HashSet<&str> = HashSet::new();
        for id in BOOT_OBSERVABILITY_CASES {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::Build], "case {id}");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id} must be an Eval case"
            );
            assert!(case.extra_argv().is_empty(), "case {id} takes no extra argv");
            assert!(case.env().is_empty(), "case {id} pins no environment");
            assert!(
                !case.description.is_empty(),
                "case {id} has no description"
            );
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {id} asserts stdout"));
            assert_eq!(case.golden.exit_code, 0, "case {id} exits 0");
            assert!(
                golden.starts_with("ok "),
                "case {id}: a Probe::Value golden is an `ok <value>` line, got {golden:?}"
            );
            assert!(
                NEW_GOLDENS.contains(&golden),
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
            used.insert(golden);
        }
        for golden in NEW_GOLDENS {
            assert!(
                used.contains(golden),
                "the reviewed golden set lists an unused golden: {golden:?}"
            );
        }
    }

    #[test]
    fn boot_pa_case_carries_the_pa_argument() {
        let cases = group();
        let cases = cases.as_slice();
        let case = case(cases, BOOT_PA_CASES[0]);
        assert_eq!(case.contracts, &[Contract::Build], "the -pa case");
        assert_eq!(
            case.extra_argv(),
            &["-pa", "/nonexistent/marker"],
            "the -pa case must pass the code-path switch it asserts"
        );
        assert_eq!(case.golden.stdout, Some("ok true"), "the -pa case");
        assert_eq!(case.golden.exit_code, 0, "the -pa case");
        let CaseKind::Eval { expr, probe, .. } = case.kind else {
            panic!("the -pa case must be an Eval case");
        };
        assert_eq!(probe, Probe::Value, "the -pa case");
        assert!(
            expr.contains("init:get_argument(pa)"),
            "the -pa case must observe the `pa` init argument: {expr:?}"
        );
        assert!(
            expr.contains("/nonexistent/marker"),
            "the -pa case must observe the path the argv supplied: {expr:?}"
        );
    }

    /// Every boot-observability expression avoids the probe wrapper's bound names (`V`, `C`,
    /// `R`): an expression that rebound one of them would make the *oracle* reject the wrapped
    /// probe (`unsafe_var`), a guaranteed false mismatch rather than a capability gap.
    #[test]
    fn boot_observability_expressions_do_not_shadow_the_probe_variables() {
        let cases = group();
        let cases = cases.as_slice();
        for id in BOOT_OBSERVABILITY_CASES.iter().chain(BOOT_PA_CASES) {
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
