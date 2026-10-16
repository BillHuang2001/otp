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

//! Contract **A.9** — preloaded/boot, expanded: boot-script-progress and
//! preloaded-module-invariant cases observed through the booted runtime.
//!
//! A.9 is "the system can build and start itself". Its black-box residue beyond the
//! version banner and the code path (the sibling `boot`/`boot_prereq` groups) is the
//! **state the boot script leaves behind**: the frozen `erlang:pre_loaded/0` module set and
//! its invariants, the code server's view of every preloaded module, the `init`/boot status
//! and its recorded arguments, the kernel processes the boot script registered, the
//! applications it started, and the version/emulator constants a booted system reports.
//! Beyond that state, the group pins the booted system's **introspection floor**: the
//! `function_exported/3` answers for the preloaded-module set, the whole-set invariants of the
//! preloaded floor (all sticky, all in `code:all_loaded/0`), the kernel application's
//! registered-name manifest, and the live floors of the preloaded `persistent_term`/`atomics`
//! modules. It also completes the per-member coverage of that floor: membership of every
//! preloaded module in `erlang:pre_loaded/0`, `code:is_loaded/1` and `code:which/1` for each
//! remaining preloaded module, `code:is_sticky/1` across the boot-loaded module set, the
//! `module_info(exports)` membership queries for `lists`/`init`, each key of the kernel
//! application's `.app` resource (`applications`/`included_applications`/
//! `optional_applications`/`start_phases`/`maxT`/`modules`/`registered`), the
//! `application:get_all_key/1` key manifest, the absent `init` arguments
//! (`mode`/`console`/`pa`), and the emulator's scheduling/limit constants
//! (`multi_scheduling`, `multi_scheduling_blockers`, `break_ignored`, `thread_pool_size`,
//! `port_limit`, `process_limit`, `fullsweep_after`).
//!
//! Every assertion is **structural** or a **release-stable value** wherever the value is
//! machine/release-dependent (the oracle is OTP 29 while the rewrite targets OTP 30, so the
//! `otp_release`/`system_version` *string* is never pinned by value — only its shape). The
//! frozen build constants that both runtimes must report for `erts-17.1`
//! (`erlang:system_info(version)`) *are* pinned exactly. No case pins a pid/ref/creation or a
//! multi-key-map `~w` rendering.
//!
//! Most cases are plain `-eval` cases ([`CaseKind::Eval`](super::CaseKind::Eval)) with a
//! [`Probe::Value`](super::Probe::Value) probe, so both runtimes receive identical argv. The
//! one `boot_extra/boot_flag_via_erl_aflags` case drives the `-boot` init argument (contract
//! A.6) through the environment half ([`CaseKind::EvalEnv`](super::CaseKind::EvalEnv)), pinning
//! that an `ERL_AFLAGS`-supplied `-boot` reaches `init:get_argument(boot)` identically.
//!
//! Beyond that state, the group pins the booted system's **installation view** — the surfaces the
//! boot chain reads out of the running installation rather than out of the emulator: the
//! `application` resource of a started application (`get_all_key/1`, `get_key/1,2`,
//! `get_application/0,1`), `init`'s automatic `progname`/`bindir` arguments, `init:script_id/0`,
//! `code:lib_dir/2`, and `filelib`'s existence probes on a guaranteed-absent path *and on the
//! installation's own directories* (`code:root_dir/0`, `code:lib_dir/0,2`, a resolved object file).
//! Every one of
//! them is pinned **structurally** where the value is host- or release-dependent (a path, a version
//! string, an invocation name) and by value only where the answer is a frozen constant, a boolean
//! or a documented `undefined`.
//!
//! The same block completes the booted system's **builtin/export floor**: `erlang:is_builtin/3`
//! (the loader's own BIF table — never a module export) and `erlang:function_exported/3` for the
//! remaining preloaded modules (`persistent_term`/`atomics`/`erts_code_purger`).
//!
//! A final widened block completes that installation view and the preloaded runtime floor: the
//! **installation hierarchy** relations (`code:root_dir/0` is absolute and prefixes the library
//! root, `code:lib_dir/0` prefixes — and differs from — the `erts` application directory), the
//! boot banner's frozen `"Erlang/OTP"` product-name prefix, the `.app` resources' non-empty
//! `description`/`vsn`/manifest members, the code server's frozen `{error,nofile}` load refusal,
//! the whole put/get floor of the preloaded `atomics`/`counters`/`persistent_term` modules, and
//! the emulator's release/limit constants (`compat_rel`, `ets_limit`, `scheduler_id`,
//! `min_bin_vheap_size`, `os_type`, `endian`). Every path is pinned only by a *relation* or a
//! *shape*; no host path, release string or live counter is ever asserted by value.
//!
//! The same block completes the code server's **load surface**: `code:load_binary/3`'s two
//! distinguishable refusals — a corrupt image ⇒ the frozen `{error,badfile}`, a *valid* image for
//! a module loaded from a sticky directory ⇒ `{error,sticky_directory}` (the image is validated
//! *before* the sticky check) — and the **boot-time loaded set**'s atom consequence: the
//! `{primLoad, …}` batches *after* `{kernel_load_completed}` are not loaded eagerly, so the
//! modules they name are not loaded at boot and the atoms only their images carry do not exist
//! (`erlang:list_to_existing_atom("hello")`, which rides `auth.beam`, is a `badarg`).
//! `code:load_binary/3`'s refusal paths also **log** asynchronously on the oracle's stdout
//! (`beam_load.c`'s report), which would race with the compared stdout, so that one case runs both
//! runtimes with the documented `-kernel logger_level none` kill switch (see [`eval_with_argv`])
//! and pins the return value alone.
//!
//! The surfaces deliberately **not** pinned here are plain differing observables — genuine
//! divergences that would be a `mismatch`, not a capability gap — and are handoffs to `beam_rt`
//! (the oracle value state, verified against the live system ERTS, is what the runtime should
//! converge on):
//!
//! | surface | oracle (OTP 29 / erts-17.1) | rust runtime |
//! |---|---|---|
//! | `code:get_path/0` | the installation's path list | a shorter, differing path list |
//! | `init:get_arguments/0` | every flag **plus** the emulator's `root`/`bindir`/`progname`/`home` | the runtime's own modelled flags only |
//! | `application:get_application/1` on a **pid** | `{ok,App}`, or `undefined` for a process outside any application | raises `function_clause` (the runtime runs no application-owned process; the *module* form **does** agree and is pinned below) |
//! | `application:get_all_key/1`'s / `get_key(App, env)`'s `env` member | the application's environment | a differing (shorter, per-VM-start-ordered) list — the *key names* are pinned, the member is not |
//! | `application:get_key(erts, vsn)` | `undefined` (erts is not a *loaded* application) | `{ok,"17.1"}` — the `kernel`/`stdlib` forms agree and are pinned |
//! | `erlang:system_info(emu_type)` / `(debug_compiled)` | the `opt` build flavour / `false` | the runtime's own `debug` flavour / `true` (only the type is pinned) |
//! | `erlang:module_info/1`, `erlang:get_module_info/2` | the export table (from `erlang`) | `undef` / `badarg` |
//! | `lists:module_info(exports)` | the export set | the same set in a differing order |
//! | `erlang:module_loaded(kernel)` | `true` | `false` |
//! | `erl_prim_loader:get_path/0` | `{ok,Paths}` | raises `badarg` |
//! | `erlang:system_info(logical_processors)` | 16 | 14 |
//! | `erlang:system_info(heap_sizes)` | the sizing list | a list with one extra entry |
//! | `erlang:function_exported/3` on `kernel` / `code` | `true` | `false` |
//! | `erlang:system_info(schedulers\|dirty_cpu_schedulers\|dirty_io_schedulers)` | the host's scheduler counts | different counts |
//! | `persistent_term:info/0` | the live `count`/`memory` counters | zero-valued counters |
//!
//! (`code:get_mode/0`, `application:get_application/0` and `init:get_argument(root\|home)` — the
//! emulator's automatic flags — **do** agree and are pinned in the sibling `boot_prereq` group.
//! `erlang:system_info(endian)` **does** agree too — `little` in both — and is pinned by the
//! oracle-backed `beam_rt` `system_info_oracle` test. `erlang:system_info(loaded)`,
//! `erlang:system_info(check_io)` and `erlang:system_info(build_type)` are capability gaps — the
//! runtime raises its documented `unsupported` (`build_type` because this runtime's own build
//! tier is not the oracle's release tag) — so they are neither cases nor mismatches.) The
//! one capability gap this group *does* carry is `init:script_id/0`: the runtime reports it as
//! `unsupported` on the `-eval`/`run` path (no boot script is loaded there), so its case is
//! recorded in `known-gaps.txt` and the ratchet can only tighten when the gap closes.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.9 boot-observability case: evaluate `expr` under both runtimes with the same
/// `-eval` argv and pin the oracle's `~w` rendering.
fn eval(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    eval_with_argv(id, description, expr, &[], golden)
}

/// The same, for the one case that needs an **extra flag** — still the *identical* argv for both
/// runtimes (`-noshell`, the flag, `-eval`). `boot_extra/code_load_binary_shapes` uses it to run
/// the emulator's documented `-kernel logger_level none` kill switch: the oracle's refusal paths
/// for `code:load_binary/3` **log** asynchronously to stdout (`beam_load.c`'s
/// `corrupt file header` / "resides in sticky dir" report), which races with the probe's own
/// output and is not part of the BIF's return value — the harness compares stdout exactly, so the
/// report must not be produced at all. (The sibling oracle harnesses suppress it the same way,
/// e.g. `crates/beam_process/tests/common/mod.rs`.)
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

/// The expanded A.9 preloaded/boot cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- The frozen `erlang:pre_loaded/0` set and its invariants ---------------------
        // The preloaded set is the minimal module floor the emulator can start without a code
        // server. Its *contents* are pinned exactly by the sibling `boot_prereq` group; this
        // group pins the **invariants of the set itself** (no duplicates, atoms only, stable
        // across calls) and **membership** of the boot-critical modules.
        eval(
            "boot_extra/pre_loaded_no_duplicates",
            "erlang:pre_loaded/0 has no duplicate members (usort equals sort)",
            "begin X = erlang:pre_loaded(), lists:usort(X) =:= lists:sort(X) end",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_length_matches_usort_length",
            "erlang:pre_loaded/0's length equals the length of its de-duplicated set",
            "length(erlang:pre_loaded()) =:= length(lists:usort(erlang:pre_loaded()))",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_all_members_are_atoms",
            "every member of erlang:pre_loaded/0 is a module atom",
            "lists:all(fun erlang:is_atom/1, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_is_stable_across_calls",
            "erlang:pre_loaded/0 returns the identical set on a second call",
            "erlang:pre_loaded() =:= erlang:pre_loaded()",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erlang",
            "the `erlang` module is preloaded",
            "lists:member(erlang, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_init",
            "the `init` module is preloaded",
            "lists:member(init, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erts_internal",
            "the `erts_internal` module is preloaded",
            "lists:member(erts_internal, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_file",
            "the `prim_file` module is preloaded",
            "lists:member(prim_file, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_zlib",
            "the `zlib` module is preloaded",
            "lists:member(zlib, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erl_prim_loader",
            "the boot loader `erl_prim_loader` is preloaded",
            "lists:member(erl_prim_loader, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_persistent_term",
            "the `persistent_term` module is preloaded",
            "lists:member(persistent_term, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_socket",
            "the `prim_socket` module is preloaded",
            "lists:member(prim_socket, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_socket_registry",
            "the `socket_registry` module is preloaded",
            "lists:member(socket_registry, erlang:pre_loaded())",
            "ok true",
        ),
        // --- The code server's view of the preloaded set ---------------------------------
        // Every module in the preloaded set is registered by the code server as
        // `{file,preloaded}` and resolved by code:which/1 to the atom `preloaded`. This is the
        // observable the real boot chain relies on before the code server can read a file.
        eval(
            "boot_extra/code_is_loaded_every_preloaded_module",
            "code:is_loaded/1 reports {file,preloaded} for every member of erlang:pre_loaded/0",
            "lists:all(fun(M) -> code:is_loaded(M) =:= {file,preloaded} end, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/code_which_every_preloaded_module",
            "code:which/1 reports the atom `preloaded` for every member of erlang:pre_loaded/0",
            "lists:all(fun(M) -> code:which(M) =:= preloaded end, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/code_is_loaded_erlang_is_preloaded",
            "code:is_loaded(erlang) is {file,preloaded}",
            "code:is_loaded(erlang)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_erts_internal_is_preloaded",
            "code:is_loaded(erts_internal) is {file,preloaded}",
            "code:is_loaded(erts_internal)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_prim_file_is_preloaded",
            "code:is_loaded(prim_file) is {file,preloaded}",
            "code:is_loaded(prim_file)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_zlib_is_preloaded",
            "code:is_loaded(zlib) is {file,preloaded}",
            "code:is_loaded(zlib)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_prim_inet_is_preloaded",
            "code:is_loaded(prim_inet) is {file,preloaded}",
            "code:is_loaded(prim_inet)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_kernel_is_file",
            "code:is_loaded/1 reports a {file,Path} pair for kernel once it is loaded",
            "begin code:ensure_loaded(kernel), L = code:is_loaded(kernel), is_tuple(L) andalso element(1, L) =:= file andalso is_list(element(2, L)) end",
            "ok true",
        ),
        eval(
            "boot_extra/code_which_init_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the booted `init` module",
            "code:which(init)",
            "ok preloaded",
        ),
        // --- init / boot status and its recorded arguments --------------------------------
        // `init` is the process the boot script itself starts; its status and the arguments it
        // recorded from the command line are the booted system's own self-description.
        eval(
            "boot_extra/init_get_status_state_is_starting",
            "init:get_status/0 reports the `starting` phase in its first element",
            "element(1, init:get_status()) =:= starting",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_status_second_element_is_atom",
            "init:get_status/0's second element is an atom (the started phase)",
            "is_atom(element(2, init:get_status()))",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_arguments_is_list_of_pairs",
            "init:get_arguments/0 is a list of {Flag,Values} pairs with atom tags and list values",
            "lists:all(fun({K,V}) -> is_atom(K) andalso is_list(V) end, init:get_arguments())",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_plain_arguments_is_list",
            "init:get_plain_arguments/0 is a list of strings",
            "is_list(init:get_plain_arguments())",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_argument_noshell_is_ok",
            "the -noshell flag the harness always passes is recorded as {ok,[[]]}",
            "init:get_argument(noshell) =:= {ok,[[]]}",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_argument_boot_absent_is_error",
            "with no -boot given, init:get_argument(boot) is the atom `error`",
            "init:get_argument(boot)",
            "ok error",
        ),
        // --- The kernel processes the boot script registered ------------------------------
        // `init` is a named kernel process the boot script relies on; it is observable through
        // both whereis/1 and the registered/0 table. (The runtime's native code/init floor does
        // not register code_server/erl_prim_loader/kernel_sup, so those are handoffs, not cases.)
        eval(
            "boot_extra/init_process_is_registered",
            "the booted `init` process is a live registered pid",
            "is_pid(whereis(init))",
            "ok true",
        ),
        eval(
            "boot_extra/init_is_in_registered",
            "the booted `init` process appears in the registered/0 name table",
            "lists:member(init, registered())",
            "ok true",
        ),
        eval(
            "boot_extra/registered_members_are_atoms",
            "every name in registered/0 is an atom",
            "lists:all(fun erlang:is_atom/1, registered())",
            "ok true",
        ),
        // --- The applications the boot script started -------------------------------------
        // kernel and stdlib are the two applications every booted system starts (`start_clean`);
        // their presence is the boot script's progress observable.
        eval(
            "boot_extra/which_applications_includes_stdlib",
            "the boot script has started `stdlib` (it appears in which_applications/0)",
            "lists:keymember(stdlib, 1, application:which_applications())",
            "ok true",
        ),
        eval(
            "boot_extra/loaded_applications_includes_stdlib",
            "the boot script has loaded `stdlib` (it appears in loaded_applications/0)",
            "lists:keymember(stdlib, 1, application:loaded_applications())",
            "ok true",
        ),
        eval(
            "boot_extra/loaded_applications_has_app_names",
            "every loaded_applications/0 entry is tagged by an application atom",
            "is_list([A || {A,_,_} <- application:loaded_applications()])",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_env_missing_key_is_undefined",
            "application:get_env/2 reports `undefined` for an unset kernel key",
            "application:get_env(kernel, a_nonexistent_key_xyz) =:= undefined",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_vsn_shape",
            "application:get_key(kernel, vsn) carries a string value (shape, not the release version)",
            "is_list(element(2, application:get_key(kernel, vsn)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_mod",
            "application:get_key(kernel, mod) is the {kernel,[]} boot module descriptor",
            "application:get_key(kernel, mod) =:= {ok,{kernel,[]}}",
            "ok true",
        ),
        // --- Version / emulator observability of the booted system ------------------------
        // The frozen build constant (`version` = "17.1") is pinned exactly; the release-varying
        // OTP release string is pinned by shape only (oracle OTP 29 vs the rewrite's OTP 30), and
        // the live table counts are pinned as a positive integer.
        eval(
            "boot_extra/system_info_version_is_frozen_17_1",
            "erlang:system_info(version) reports the frozen erts version string \"17.1\"",
            "erlang:system_info(version) =:= \"17.1\"",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_otp_release_is_numeric_string",
            "erlang:system_info(otp_release) is a string of ASCII digits (value is release-dependent)",
            "lists:all(fun(C) -> C >= $0 andalso C =< $9 end, erlang:system_info(otp_release))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_otp_release_is_non_empty",
            "erlang:system_info(otp_release) is a non-empty string",
            "length(erlang:system_info(otp_release)) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_atom_count_is_positive_integer",
            "erlang:system_info(atom_count) is a positive integer (the live atom-table size)",
            "is_integer(erlang:system_info(atom_count)) andalso erlang:system_info(atom_count) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_creation_is_integer",
            "erlang:system_info(creation) is an integer (the per-VM-start creation stamp)",
            "is_integer(erlang:system_info(creation))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_port_count_is_integer",
            "erlang:system_info(port_count) is an integer count",
            "is_integer(erlang:system_info(port_count))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_ets_count_is_integer",
            "erlang:system_info(ets_count) is an integer count",
            "is_integer(erlang:system_info(ets_count))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_scheduler_bind_type_is_atom",
            "erlang:system_info(scheduler_bind_type) is an atom",
            "is_atom(erlang:system_info(scheduler_bind_type))",
            "ok true",
        ),
        // --- Preloaded-module introspection ----------------------------------------------
        // A preloaded module is still a real module: its `module_info/1` export table is
        // observable. (`init` is used because the runtime's native `erlang` floor does not
        // implement `erlang:module_info/1` — that divergence is a handoff, not a case.)
        eval(
            "boot_extra/init_module_info_exports_is_list",
            "init:module_info(exports) is a list (a preloaded module's export table)",
            "is_list(init:module_info(exports))",
            "ok true",
        ),
        eval(
            "boot_extra/init_module_info_exports_non_empty",
            "the preloaded `init` module exports at least one function",
            "length(init:module_info(exports)) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/init_module_info_exports_are_fa_pairs",
            "every init:module_info(exports) entry is an {Atom,Arity} pair",
            "lists:all(fun({F,A}) -> is_atom(F) andalso is_integer(A) end, init:module_info(exports))",
            "ok true",
        ),
        // --- The code server's view of the booted module universe -------------------------
        // Once the boot script has run, the code server can answer for the whole module universe:
        // where the root is, which module lives in which file, what is loaded, and what the code
        // path is. Only the *shapes* and the frozen constants are pinned — an object path is an
        // absolute path of the host installation and is never asserted by value.
        eval(
            "boot_extra/code_objfile_extension_is_beam",
            "code:objfile_extension/0 reports the frozen object-file extension \".beam\"",
            "code:objfile_extension() =:= \".beam\"",
            "ok true",
        ),
        eval(
            "boot_extra/code_root_dir_is_non_empty_string",
            "code:root_dir/0 is a non-empty string (the installation root, never pinned by value)",
            "begin D = code:root_dir(), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/code_lib_dir_kernel_is_non_empty_string",
            "code:lib_dir/1 is a non-empty string for the booted `kernel` application",
            "begin D = code:lib_dir(kernel), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/code_lib_dir_unknown_application_is_bad_name",
            "code:lib_dir/1 reports {error,bad_name} for an application that does not exist",
            "code:lib_dir(nonexisting_module_xyz)",
            "ok {error,bad_name}",
        ),
        eval(
            "boot_extra/code_which_unknown_module_is_non_existing",
            "code:which/1 reports the atom `non_existing` for a module that was never loaded",
            "code:which(no_such_module_xyz)",
            "ok non_existing",
        ),
        eval(
            "boot_extra/code_which_erlang_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the `erlang` module",
            "code:which(erlang)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_which_lists_is_a_beam_object_path",
            "code:which/1 reports a string ending in \".beam\" for the loaded `lists` module",
            "begin P = code:which(lists), is_list(P) andalso filename:extension(P) =:= \".beam\" end",
            "ok true",
        ),
        eval(
            "boot_extra/code_is_loaded_unknown_module_is_false",
            "code:is_loaded/1 reports the atom `false` for a module that was never loaded",
            "code:is_loaded(no_such_module_xyz)",
            "ok false",
        ),
        eval(
            "boot_extra/code_is_loaded_lists_is_a_file_pair",
            "code:is_loaded/1 reports a {file,Path} pair for lists once it is loaded",
            "begin code:ensure_loaded(lists), L = code:is_loaded(lists), is_tuple(L) andalso element(1, L) =:= file andalso is_list(element(2, L)) end",
            "ok true",
        ),
        eval(
            "boot_extra/code_all_loaded_entries_are_module_file_pairs",
            "every code:all_loaded/0 entry is a 2-tuple tagged by a module atom",
            "lists:all(fun(X) -> is_tuple(X) andalso tuple_size(X) =:= 2 andalso is_atom(element(1, X)) end, code:all_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/code_get_path_is_a_list_of_strings",
            "every code:get_path/0 entry is a string (the paths themselves are host-specific)",
            "lists:all(fun erlang:is_list/1, code:get_path())",
            "ok true",
        ),
        eval(
            "boot_extra/code_ensure_loaded_lists_is_module_lists",
            "code:ensure_loaded/1 on the already-loaded `lists` module is idempotent",
            "code:ensure_loaded(lists)",
            "ok {module,lists}",
        ),
        eval(
            "boot_extra/code_is_sticky_kernel_is_true",
            "code:is_sticky/1 reports the boot-loaded `kernel` module as sticky",
            "code:is_sticky(kernel)",
            "ok true",
        ),
        // --- The init/boot argument surface the boot script itself relies on --------------
        // `init`'s own status and the argument table it recorded from the boot command line; the
        // full `init:get_arguments/0` rendering is pinned by shape in the section above.
        eval(
            "boot_extra/init_get_status_is_starting_started",
            "init:get_status/0 reports the {starting,started} booted phase pair",
            "init:get_status()",
            "ok {starting,started}",
        ),
        eval(
            "boot_extra/init_get_arguments_records_the_noshell_flag",
            "the -noshell flag the harness always passes is present in init:get_arguments/0",
            "lists:keymember(noshell, 1, init:get_arguments())",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_argument_absent_flag_is_error",
            "init:get_argument/1 reports the atom `error` for a flag that was never passed",
            "init:get_argument(some_flag_xyz_absent)",
            "ok error",
        ),
        // --- The applications the boot script started -------------------------------------
        // `loaded_applications/0` carries release-varying version strings, so only its shape is
        // pinned; `which_applications/0` is observed by membership.
        eval(
            "boot_extra/loaded_applications_entries_are_three_tuples",
            "every loaded_applications/0 entry is {AppAtom,Description,String}",
            "lists:all(fun({A,V,D}) -> is_atom(A) andalso is_list(V) andalso is_list(D) end, application:loaded_applications())",
            "ok true",
        ),
        eval(
            "boot_extra/which_applications_kernel_is_running",
            "the boot script has started `kernel` (it appears in which_applications/0)",
            "lists:keymember(kernel, 1, application:which_applications())",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_description_shape",
            "application:get_key(kernel, description) carries a string value",
            "is_list(element(2, application:get_key(kernel, description)))",
            "ok true",
        ),
        // --- Module introspection of boot-loaded modules ----------------------------------
        // `module_info/1` is a real export of every loaded module; its `module` key is stable while
        // the *order* of the `exports` list is not (it is pinned by membership, never by value).
        eval(
            "boot_extra/lists_module_info_module_is_lists",
            "lists:module_info(module) reports the module's own name",
            "lists:module_info(module)",
            "ok lists",
        ),
        eval(
            "boot_extra/init_module_info_module_is_init",
            "init:module_info(module) reports the module's own name",
            "init:module_info(module)",
            "ok init",
        ),
        eval(
            "boot_extra/lists_module_info_exports_contains_sort_1",
            "the `lists` module's export table contains the {sort,1} entry",
            "lists:member({sort,1}, lists:module_info(exports))",
            "ok true",
        ),
        // `function_exported/3` answers out of the *loaded* export table, so the case makes the
        // load state explicit (`lists` is not a declared `bif.tab` builtin — unlike
        // `lists:member/2`, which `bifs/function_exported_lists_member` exercises — so it is only
        // exported once the module is loaded). `code:ensure_loaded/1` forces that load on both
        // sides; `lists:module_info/1` in the sibling case above does the same implicitly.
        eval(
            "boot_extra/function_exported_lists_sort_1_is_true",
            "erlang:function_exported/3 reports lists:sort/1 as exported once lists is loaded",
            "case code:ensure_loaded(lists) of {module,_} -> erlang:function_exported(lists, sort, 1) end",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_unknown_function_is_false",
            "erlang:function_exported/3 reports a nonexistent function as not exported",
            "erlang:function_exported(lists, no_such_fun_xyz, 1)",
            "ok false",
        ),
        eval(
            "boot_extra/module_loaded_unknown_module_is_false",
            "erlang:module_loaded/1 reports a module that was never loaded as not loaded",
            "erlang:module_loaded(no_such_module_xyz)",
            "ok false",
        ),
        // --- Emulator constants of the booted system --------------------------------------
        // The frozen emulator constants both runtimes must report are pinned exactly; the
        // build-flavour / machine-dependent values are pinned by shape only.
        eval(
            "boot_extra/system_info_machine_is_beam",
            "erlang:system_info(machine) reports the frozen emulator family \"BEAM\"",
            "erlang:system_info(machine) =:= \"BEAM\"",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_emu_flavor_is_atom",
            "erlang:system_info(emu_flavor) is an atom (jit/emu — the build flavour)",
            "is_atom(erlang:system_info(emu_flavor))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_heap_type_is_atom",
            "erlang:system_info(heap_type) is an atom (the process heap flavour)",
            "is_atom(erlang:system_info(heap_type))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_smp_support_is_true",
            "erlang:system_info(smp_support) reports a multi-scheduler-capable emulator",
            "erlang:system_info(smp_support)",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_threads_is_true",
            "erlang:system_info(threads) reports a thread-enabled emulator",
            "erlang:system_info(threads)",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_debug_compiled_is_boolean",
            "erlang:system_info(debug_compiled) is a boolean build flag (true for a debug build, false for a release build)",
            "is_boolean(erlang:system_info(debug_compiled))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_nif_version_is_a_string",
            "erlang:system_info(nif_version) is a string (value is release-dependent)",
            "is_list(erlang:system_info(nif_version))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_driver_version_is_a_string",
            "erlang:system_info(driver_version) is a string",
            "is_list(erlang:system_info(driver_version))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_atom_limit_is_positive_integer",
            "erlang:system_info(atom_limit) is a positive integer limit",
            "begin N = erlang:system_info(atom_limit), is_integer(N) andalso N > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_min_heap_size_pair_shape",
            "erlang:system_info(min_heap_size) is the {min_heap_size,Words} pair",
            "begin M = erlang:system_info(min_heap_size), is_tuple(M) andalso element(1, M) =:= min_heap_size andalso is_integer(element(2, M)) end",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_schedulers_online_is_positive",
            "erlang:system_info(schedulers_online) is a positive scheduler count (value is host-dependent)",
            "erlang:system_info(schedulers_online) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_system_architecture_is_a_string",
            "erlang:system_info(system_architecture) is a string (the target triple, host-dependent)",
            "is_list(erlang:system_info(system_architecture))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_system_version_is_a_string",
            "erlang:system_info(system_version) is a string (value is release-dependent)",
            "is_list(erlang:system_info(system_version))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_modified_timing_level_is_undefined",
            "with no timing override, erlang:system_info(modified_timing_level) is `undefined`",
            "erlang:system_info(modified_timing_level)",
            "ok undefined",
        ),
        // --- The preloaded-module runtime floor -------------------------------------------
        // `persistent_term` is preloaded (see the membership case above); its observable floor is
        // live without any application having started.
        eval(
            "boot_extra/persistent_term_missing_key_returns_the_default",
            "the preloaded `persistent_term` serves get/2's default for an unset key",
            "persistent_term:get(nonexistent_key_xyz, default)",
            "ok default",
        ),
        // --- The `-boot` init argument, through the environment half (contract A.6) --------
        // The `-boot FILE` switch is what selects the boot script; it is recorded by `init`
        // identically whether it arrives on the command line or through `ERL_AFLAGS`. The
        // command-line half is pinned by the sibling `boot_prereq` group; this case pins the
        // same init argument reaching `init:get_argument(boot)` through the environment merge.
        Case {
            id: "boot_extra/boot_flag_via_erl_aflags",
            description: "an ERL_AFLAGS-supplied `-boot start` reaches init:get_argument(boot) identically",
            contracts: &[Contract::Build],
            kind: CaseKind::EvalEnv {
                expr: "init:get_argument(boot) =:= {ok, [[\"start\"]]}",
                extra_argv: &[],
                env: &[("ERL_AFLAGS", "-boot start")],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- The preloaded-module export floor (`function_exported/3`) --------------------
        // The preloaded set is *loaded*, so its export tables answer before any application
        // has started: this is the boot chain's own introspection floor. (`kernel`/`code` are
        // handoffs instead: the runtime answers `false` there where the oracle answers `true`,
        // so see the divergence table above rather than a case.)
        eval(
            "boot_extra/function_exported_erlang_system_info_1",
            "the preloaded `erlang` module exports system_info/1",
            "erlang:function_exported(erlang, system_info, 1)",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_init_get_status_0",
            "the preloaded `init` module exports get_status/0",
            "erlang:function_exported(init, get_status, 0)",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_erts_internal_cmp_term_2",
            "the preloaded `erts_internal` module exports cmp_term/2",
            "erlang:function_exported(erts_internal, cmp_term, 2)",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_prim_file_read_file_1",
            "the preloaded `prim_file` module exports read_file/1",
            "erlang:function_exported(prim_file, read_file, 1)",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_erl_prim_loader_get_path_0",
            "the preloaded `erl_prim_loader` module exports get_path/0",
            "erlang:function_exported(erl_prim_loader, get_path, 0)",
            "ok true",
        ),
        // --- The code server's view of the remaining preloaded modules -------------------
        // The sibling section above covers the first half of the preloaded set; these pin the
        // same `{file,preloaded}` / `preloaded` invariants for further members of the frozen 22.
        eval(
            "boot_extra/code_which_atomics_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the preloaded `atomics` module",
            "code:which(atomics)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_which_counters_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the preloaded `counters` module",
            "code:which(counters)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_is_loaded_persistent_term_is_preloaded",
            "code:is_loaded(persistent_term) is the {file,preloaded} pair",
            "code:is_loaded(persistent_term)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_is_loaded_erl_tracer_is_preloaded",
            "code:is_loaded(erl_tracer) is the {file,preloaded} pair",
            "code:is_loaded(erl_tracer)",
            "ok {file,preloaded}",
        ),
        // --- Whole-set invariants of the preloaded floor ---------------------------------
        // The boot chain relies on *every* preloaded module being sticky (it may not be
        // purged) and present in the code server's loaded registry. These are set-wide
        // queries, so a single extra/missing member cannot slip past.
        eval(
            "boot_extra/pre_loaded_every_module_is_sticky",
            "code:is_sticky/1 reports every member of erlang:pre_loaded/0 as sticky",
            "lists:all(fun(M) -> code:is_sticky(M) end, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_every_module_is_in_all_loaded",
            "every member of erlang:pre_loaded/0 appears in code:all_loaded/0",
            "lists:all(fun(M) -> lists:keymember(M, 1, code:all_loaded()) end, erlang:pre_loaded())",
            "ok true",
        ),
        // --- The kernel application's registered-name table -------------------------------
        // `application:get_key(kernel, registered)` is the boot script's own kernel-process
        // manifest; its shape (atoms only) and the `application_controller` entry it names are
        // the boot progress observable, without pinning any per-VM pid.
        eval(
            "boot_extra/application_get_key_kernel_registered_is_atom_list",
            "application:get_key(kernel, registered) is a list of registered-name atoms",
            "lists:all(fun erlang:is_atom/1, element(2, application:get_key(kernel, registered)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_registered_contains_application_controller",
            "the kernel application's registered-name table names `application_controller`",
            "lists:member(application_controller, element(2, application:get_key(kernel, registered)))",
            "ok true",
        ),
        // --- Further booted-system constants and the preloaded runtime floor --------------
        eval(
            "boot_extra/system_info_wordsize_is_eight",
            "erlang:system_info(wordsize) is the frozen 8-byte machine word of the 64-bit target",
            "erlang:system_info(wordsize) =:= 8",
            "ok true",
        ),
        eval(
            "boot_extra/persistent_term_put_get_round_trip",
            "the preloaded `persistent_term` round-trips a put/get without an application start",
            "begin persistent_term:put(boot_extra_key, 42), persistent_term:get(boot_extra_key) =:= 42 end",
            "ok true",
        ),
        eval(
            "boot_extra/atomics_get_initial_is_zero",
            "the preloaded `atomics` module serves a fresh 1-slot array whose initial value is 0",
            "atomics:get(atomics:new(1, []), 1) =:= 0",
            "ok true",
        ),
        // --- The `application` resource of a started application --------------------------
        // `get_all_key/1`/`get_key/2` read the *parsed `.app` resource* of an application the boot
        // script started; `get_application/0,1` resolves the application of a process (or of a
        // module). The returned key/value list carries volatile members (the `env` list's entries
        // and per-VM-start order) and release-varying strings (the `vsn`), so only the **shape**,
        // the **presence of specific keys**, the `undefined` answers and the module → application
        // map are pinned — never a raw rendering of the whole list.
        eval(
            "boot_extra/application_get_all_key_kernel_is_key_value_list",
            "application:get_all_key(kernel) is {ok,KeyValueList} of {atom(),_} pairs",
            "begin {ok,L} = application:get_all_key(kernel), is_list(L) andalso length(L) > 0 andalso lists:all(fun({K,_}) -> is_atom(K) end, L) end",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_stdlib_is_key_value_list",
            "application:get_all_key(stdlib) is a non-empty {ok,KeyValueList}",
            "begin {ok,L} = application:get_all_key(stdlib), is_list(L) andalso length(L) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_kernel_has_mod_key",
            "application:get_all_key(kernel) carries the application's `mod` start module",
            "lists:keymember(mod, 1, element(2, application:get_all_key(kernel)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_kernel_has_vsn_key",
            "application:get_all_key(kernel)'s key names contain `vsn`",
            "begin {ok,L} = application:get_all_key(kernel), lists:member(vsn, [K || {K,_} <- L]) end",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_and_get_key_agree_on_registered",
            "application:get_all_key/1 and application:get_key/2 report the same `registered` manifest",
            "lists:keyfind(registered, 1, element(2, application:get_all_key(kernel))) =:= {registered, element(2, application:get_key(kernel, registered))}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_unknown_application_is_undefined",
            "application:get_all_key/1 reports `undefined` for an application that does not exist",
            "application:get_all_key(no_such_application_xyz)",
            "ok undefined",
        ),
        // `application:get_key/1` is the *arity-1* form: it looks `Key` up in the **calling
        // process's** application. The `-eval` process belongs to no application, so the oracle
        // answers `undefined` — the same value a nonexistent key gives.
        eval(
            "boot_extra/application_get_key_1_without_an_application_is_undefined",
            "application:get_key/1 is `undefined` for a process that belongs to no application",
            "application:get_key(vsn)",
            "ok undefined",
        ),
        eval(
            "boot_extra/application_get_key_kernel_applications_is_ok_empty",
            "application:get_key(kernel, applications) is the empty dependency list (kernel depends on no application)",
            "application:get_key(kernel, applications) =:= {ok,[]}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_maxP_is_infinity",
            "application:get_key(kernel, maxP) is the unbounded `infinity` process limit",
            "application:get_key(kernel, maxP) =:= {ok,infinity}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_stdlib_mod_is_ok_empty",
            "application:get_key(stdlib, mod) is {ok,[]} (a library application carries no start module)",
            "application:get_key(stdlib, mod) =:= {ok,[]}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_missing_key_is_undefined",
            "application:get_key/2 reports `undefined` for a key the application does not set",
            "application:get_key(kernel, no_such_key_xyz)",
            "ok undefined",
        ),
        eval(
            "boot_extra/application_get_key_unknown_application_is_undefined",
            "application:get_key/2 reports `undefined` for an application that does not exist",
            "application:get_key(no_such_application_xyz, vsn)",
            "ok undefined",
        ),
        eval(
            "boot_extra/application_get_application_0_is_undefined",
            "application:get_application/0 is `undefined` for a process outside any application",
            "application:get_application()",
            "ok undefined",
        ),
        eval(
            "boot_extra/application_get_application_1_kernel_module_is_kernel",
            "application:get_application/1 maps the `kernel` module to the kernel application",
            "application:get_application(kernel)",
            "ok {ok,kernel}",
        ),
        eval(
            "boot_extra/application_get_application_1_lists_module_is_stdlib",
            "application:get_application/1 maps the `lists` module to the stdlib application",
            "application:get_application(lists)",
            "ok {ok,stdlib}",
        ),
        eval(
            "boot_extra/application_get_application_1_unknown_module_is_undefined",
            "application:get_application/1 is `undefined` for a module that belongs to no loaded application",
            "application:get_application(no_such_module_xyz)",
            "ok undefined",
        ),
        // --- `init`'s automatic arguments and the boot-script identity ---------------------
        // `progname`/`bindir` are the two automatic flags whose *values* are inherently
        // host/runtime-specific (the invocation name, the installation's binary directory), so
        // only their shape — and, for `bindir`, the frozen `bin` basename — is pinned.
        eval(
            "boot_extra/init_get_argument_progname_is_a_non_empty_string",
            "init:get_argument(progname) is {ok,[[String]]} with a non-empty program name",
            "begin {ok,[[P]]} = init:get_argument(progname), is_list(P) andalso length(P) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_argument_bindir_is_a_non_empty_string",
            "init:get_argument(bindir) is {ok,[[String]]} with a non-empty binary directory",
            "begin {ok,[[D]]} = init:get_argument(bindir), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/init_get_argument_bindir_is_a_bin_directory",
            "init:get_argument(bindir)'s path ends in the frozen `bin` component",
            "begin {ok,[[D]]} = init:get_argument(bindir), filename:basename(D) =:= \"bin\" end",
            "ok true",
        ),
        // `init:script_id/0` names the loaded boot script. The `-eval` path loads none, so the
        // runtime raises its documented `unsupported`; the assertion is release-independent (the
        // oracle is OTP 29, the rewrite targets OTP 30) and the case is recorded in
        // `known-gaps.txt` so the ratchet can only tighten when the gap closes.
        eval(
            "boot_extra/init_script_id_names_the_running_otp_release",
            "init:script_id/0 is the {Name,Release} pair of the running OTP release (a capability gap today)",
            "init:script_id() =:= {\"Erlang/OTP\", erlang:system_info(otp_release)}",
            "ok true",
        ),
        // --- `code:lib_dir/2` --------------------------------------------------------------
        // `code:lib_dir/2` joins the booted application's directory with a subdirectory; the
        // joined path is host-specific, so only its shape and its relation to `lib_dir/1` are
        // pinned.
        eval(
            "boot_extra/code_lib_dir_kernel_ebin_is_the_ebin_subdirectory",
            "code:lib_dir(kernel, ebin) is a path ending in the frozen `ebin` component",
            "begin P = code:lib_dir(kernel, ebin), is_list(P) andalso filename:basename(P) =:= \"ebin\" end",
            "ok true",
        ),
        eval(
            "boot_extra/code_lib_dir_stdlib_ebin_is_the_ebin_subdirectory",
            "code:lib_dir(stdlib, ebin) is a path ending in the frozen `ebin` component",
            "begin P = code:lib_dir(stdlib, ebin), is_list(P) andalso filename:basename(P) =:= \"ebin\" end",
            "ok true",
        ),
        eval(
            "boot_extra/code_lib_dir_kernel_ebin_is_under_the_application_dir",
            "code:lib_dir(kernel, ebin) is `code:lib_dir(kernel)` plus a subdirectory",
            "lists:prefix(code:lib_dir(kernel), code:lib_dir(kernel, ebin))",
            "ok true",
        ),
        // --- `filelib`'s existence probes ---------------------------------------------------
        // The boot chain's file lookups are `filelib:is_dir/1`/`is_file/1`/`wildcard/1`; on a
        // **guaranteed-absent** path both runtimes agree on the negative answer. (An *existing*
        // path is the runtime's typed `access(2)` capability gap — a handoff, not a case.)
        eval(
            "boot_extra/filelib_is_dir_of_an_absent_path_is_false",
            "filelib:is_dir/1 is `false` for a path that does not exist",
            "filelib:is_dir(\"/nonexistent/boot_extra_absent_dir\")",
            "ok false",
        ),
        eval(
            "boot_extra/filelib_is_file_of_an_absent_path_is_false",
            "filelib:is_file/1 is `false` for a path that does not exist",
            "filelib:is_file(\"/nonexistent/boot_extra_absent_file\")",
            "ok false",
        ),
        eval(
            "boot_extra/filelib_wildcard_of_an_absent_path_is_empty",
            "filelib:wildcard/1 is the empty list for a path that does not exist",
            "filelib:wildcard(\"/nonexistent/boot_extra_absent_dir/*\")",
            "ok []",
        ),
        // --- Remaining members of the frozen preloaded set --------------------------------
        // The sections above pin membership for the first half of the preloaded set; these
        // complete it, so a single missing/extra member cannot slip past.
        eval(
            "boot_extra/pre_loaded_members_include_erl_init",
            "the boot-time `erl_init` module is preloaded",
            "lists:member(erl_init, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_buffer",
            "the `prim_buffer` module is preloaded",
            "lists:member(prim_buffer, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_eval",
            "the `prim_eval` module is preloaded",
            "lists:member(prim_eval, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_net",
            "the `prim_net` module is preloaded",
            "lists:member(prim_net, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_prim_inet",
            "the `prim_inet` module is preloaded",
            "lists:member(prim_inet, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erl_tracer",
            "the `erl_tracer` module is preloaded",
            "lists:member(erl_tracer, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erts_code_purger",
            "the `erts_code_purger` module is preloaded",
            "lists:member(erts_code_purger, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_members_include_erts_trace_cleaner",
            "the `erts_trace_cleaner` module is preloaded",
            "lists:member(erts_trace_cleaner, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_is_a_list",
            "erlang:pre_loaded/0 returns a list and not another collection",
            "is_list(erlang:pre_loaded())",
            "ok true",
        ),
        // --- code:is_loaded/1 and code:which/1 for the remaining preloaded modules -------
        // The code server must resolve *every* member of the frozen preloaded set before any
        // file is readable; these pin the remaining members individually.
        eval(
            "boot_extra/code_is_loaded_erl_init_is_preloaded",
            "code:is_loaded(erl_init) is the {file,preloaded} pair",
            "code:is_loaded(erl_init)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_which_erl_init_is_preloaded",
            "code:which/1 reports the atom `preloaded` for the preloaded `erl_init` module",
            "code:which(erl_init)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_is_loaded_erts_dirty_process_signal_handler_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for `erts_dirty_process_signal_handler`",
            "code:is_loaded(erts_dirty_process_signal_handler)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_which_erts_dirty_process_signal_handler_is_preloaded",
            "code:which/1 reports the atom `preloaded` for `erts_dirty_process_signal_handler`",
            "code:which(erts_dirty_process_signal_handler)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_is_loaded_erts_literal_area_collector_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for `erts_literal_area_collector`",
            "code:is_loaded(erts_literal_area_collector)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_which_erts_literal_area_collector_is_preloaded",
            "code:which/1 reports the atom `preloaded` for `erts_literal_area_collector`",
            "code:which(erts_literal_area_collector)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_is_loaded_erts_trace_cleaner_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for `erts_trace_cleaner`",
            "code:is_loaded(erts_trace_cleaner)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_which_erts_trace_cleaner_is_preloaded",
            "code:which/1 reports the atom `preloaded` for `erts_trace_cleaner`",
            "code:which(erts_trace_cleaner)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_is_loaded_prim_buffer_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for `prim_buffer`",
            "code:is_loaded(prim_buffer)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_which_prim_buffer_is_preloaded",
            "code:which/1 reports the atom `preloaded` for `prim_buffer`",
            "code:which(prim_buffer)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_is_loaded_prim_eval_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for `prim_eval`",
            "code:is_loaded(prim_eval)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_which_prim_eval_is_preloaded",
            "code:which/1 reports the atom `preloaded` for `prim_eval`",
            "code:which(prim_eval)",
            "ok preloaded",
        ),
        eval(
            "boot_extra/code_is_loaded_prim_socket_is_preloaded",
            "code:is_loaded/1 reports {file,preloaded} for `prim_socket`",
            "code:is_loaded(prim_socket)",
            "ok {file,preloaded}",
        ),
        eval(
            "boot_extra/code_which_prim_socket_is_preloaded",
            "code:which/1 reports the atom `preloaded` for `prim_socket`",
            "code:which(prim_socket)",
            "ok preloaded",
        ),
        // --- code:is_sticky/1 across the boot-loaded module floor -------------------------
        // Sticky modules may not be purged; the boot chain relies on the boot-critical modules
        // being sticky while a module that was never loaded is (trivially) not.
        eval(
            "boot_extra/code_is_sticky_init_is_true",
            "code:is_sticky/1 reports the booted `init` module as sticky",
            "code:is_sticky(init)",
            "ok true",
        ),
        eval(
            "boot_extra/code_is_sticky_lists_is_true",
            "code:is_sticky/1 reports the file-loaded `lists` module as sticky",
            "code:is_sticky(lists)",
            "ok true",
        ),
        eval(
            "boot_extra/code_is_sticky_unknown_module_is_false",
            "code:is_sticky/1 reports a module that was never loaded as not sticky",
            "code:is_sticky(no_such_module_xyz)",
            "ok false",
        ),
        // --- The code server's loaded registry --------------------------------------------
        eval(
            "boot_extra/code_all_loaded_is_a_list",
            "code:all_loaded/0 returns a list (the code server's loaded registry)",
            "is_list(code:all_loaded())",
            "ok true",
        ),
        // --- `erlang:module_loaded/1` for the remaining preloaded modules ----------------
        eval(
            "boot_extra/module_loaded_erl_init_is_true",
            "erlang:module_loaded/1 reports the preloaded `erl_init` module as loaded",
            "erlang:module_loaded(erl_init)",
            "ok true",
        ),
        eval(
            "boot_extra/module_loaded_prim_buffer_is_true",
            "erlang:module_loaded/1 reports the preloaded `prim_buffer` module as loaded",
            "erlang:module_loaded(prim_buffer)",
            "ok true",
        ),
        eval(
            "boot_extra/module_loaded_prim_eval_is_true",
            "erlang:module_loaded/1 reports the preloaded `prim_eval` module as loaded",
            "erlang:module_loaded(prim_eval)",
            "ok true",
        ),
        eval(
            "boot_extra/module_loaded_erts_code_purger_is_true",
            "erlang:module_loaded/1 reports the preloaded `erts_code_purger` module as loaded",
            "erlang:module_loaded(erts_code_purger)",
            "ok true",
        ),
        eval(
            "boot_extra/module_loaded_zlib_is_true",
            "erlang:module_loaded/1 reports the preloaded `zlib` module as loaded",
            "erlang:module_loaded(zlib)",
            "ok true",
        ),
        // --- The export tables of the boot-loaded modules ---------------------------------
        // `module_info(exports)` is the module's own export list; a boot target must answer it
        // for the file-loaded modules too, not just for the preloaded floor.
        eval(
            "boot_extra/lists_module_info_exports_contains_reverse_1",
            "the `lists` module's export table contains the {reverse,1} entry",
            "lists:member({reverse,1}, lists:module_info(exports))",
            "ok true",
        ),
        eval(
            "boot_extra/lists_module_info_exports_contains_map_2",
            "the `lists` module's export table contains the {map,2} entry",
            "lists:member({map,2}, lists:module_info(exports))",
            "ok true",
        ),
        eval(
            "boot_extra/lists_module_info_exports_contains_foldl_3",
            "the `lists` module's export table contains the {foldl,3} entry",
            "lists:member({foldl,3}, lists:module_info(exports))",
            "ok true",
        ),
        eval(
            "boot_extra/init_module_info_exports_contains_get_status_0",
            "the preloaded `init` module's export table contains the {get_status,0} entry",
            "lists:member({get_status,0}, init:module_info(exports))",
            "ok true",
        ),
        eval(
            "boot_extra/init_module_info_exports_contains_get_argument_1",
            "the preloaded `init` module's export table contains the {get_argument,1} entry",
            "lists:member({get_argument,1}, init:module_info(exports))",
            "ok true",
        ),
        // --- The kernel application's `.app` resource, key by key -------------------------
        // Each key the boot chain reads from an application resource is pinned by its
        // release-stable value or by membership; the version strings and host paths are never
        // asserted by value.
        eval(
            "boot_extra/application_get_key_stdlib_applications_is_kernel_only",
            "application:get_key(stdlib, applications) is exactly the kernel dependency",
            "application:get_key(stdlib, applications) =:= {ok,[kernel]}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_included_applications_is_empty",
            "application:get_key(kernel, included_applications) is the empty list",
            "application:get_key(kernel, included_applications) =:= {ok,[]}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_optional_applications_is_empty",
            "application:get_key(kernel, optional_applications) is the empty list",
            "application:get_key(kernel, optional_applications) =:= {ok,[]}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_start_phases_is_undefined",
            "application:get_key(kernel, start_phases) is {ok,undefined} (kernel declares no start phases)",
            "application:get_key(kernel, start_phases) =:= {ok,undefined}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_maxT_is_infinity",
            "application:get_key(kernel, maxT) is the unbounded `infinity` termination timeout",
            "application:get_key(kernel, maxT) =:= {ok,infinity}",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_modules_contains_kernel",
            "the kernel application's module list contains the `kernel` module",
            "lists:member(kernel, element(2, application:get_key(kernel, modules)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_modules_contains_code",
            "the kernel application's module list contains the `code` module",
            "lists:member(code, element(2, application:get_key(kernel, modules)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_stdlib_modules_contains_lists",
            "the stdlib application's module list contains the `lists` module",
            "lists:member(lists, element(2, application:get_key(stdlib, modules)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_registered_names_file_server_2",
            "the kernel registered-name manifest names `file_server_2` (the boot-time file server)",
            "lists:member(file_server_2, element(2, application:get_key(kernel, registered)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_registered_names_code_server",
            "the kernel registered-name manifest names `code_server`",
            "lists:member(code_server, element(2, application:get_key(kernel, registered)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_registered_names_kernel_sup",
            "the kernel registered-name manifest names `kernel_sup`",
            "lists:member(kernel_sup, element(2, application:get_key(kernel, registered)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_kernel_has_env_key",
            "application:get_all_key(kernel)'s key names contain `env`",
            "lists:keymember(env, 1, element(2, application:get_all_key(kernel)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_kernel_has_description_key",
            "application:get_all_key(kernel)'s key names contain `description`",
            "lists:keymember(description, 1, element(2, application:get_all_key(kernel)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_kernel_has_modules_key",
            "application:get_all_key(kernel)'s key names contain `modules`",
            "lists:keymember(modules, 1, element(2, application:get_all_key(kernel)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_kernel_has_registered_key",
            "application:get_all_key(kernel)'s key names contain `registered`",
            "lists:keymember(registered, 1, element(2, application:get_all_key(kernel)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_all_key_kernel_has_applications_key",
            "application:get_all_key(kernel)'s key names contain `applications`",
            "lists:keymember(applications, 1, element(2, application:get_all_key(kernel)))",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_application_1_file_module_is_kernel",
            "application:get_application/1 maps the kernel application's `file` module to kernel",
            "application:get_application(file)",
            "ok {ok,kernel}",
        ),
        eval(
            "boot_extra/application_get_application_1_erts_module_is_undefined",
            "application:get_application/1 is `undefined` for the `erts` module (erts is not a loaded application)",
            "application:get_application(erts)",
            "ok undefined",
        ),
        eval(
            "boot_extra/application_get_env_stdlib_restricted_shell_is_undefined",
            "application:get_env/2 reports `undefined` for an unset stdlib key",
            "application:get_env(stdlib, restricted_shell) =:= undefined",
            "ok true",
        ),
        eval(
            "boot_extra/loaded_applications_includes_kernel",
            "the boot script has loaded `kernel` (it appears in loaded_applications/0)",
            "lists:keymember(kernel, 1, application:loaded_applications())",
            "ok true",
        ),
        eval(
            "boot_extra/loaded_applications_excludes_erts",
            "`erts` is not a loaded application (it is the emulator, not an application the boot script starts)",
            "lists:keymember(erts, 1, application:loaded_applications())",
            "ok false",
        ),
        eval(
            "boot_extra/which_applications_is_a_list",
            "application:which_applications/0 returns a list",
            "is_list(application:which_applications())",
            "ok true",
        ),
        // --- `init`'s argument table for flags that were never given ----------------------
        eval(
            "boot_extra/init_get_argument_mode_is_error",
            "with no -mode given, init:get_argument(mode) is the atom `error`",
            "init:get_argument(mode)",
            "ok error",
        ),
        eval(
            "boot_extra/init_get_argument_console_is_error",
            "with no -console given, init:get_argument(console) is the atom `error`",
            "init:get_argument(console)",
            "ok error",
        ),
        eval(
            "boot_extra/init_get_argument_pa_absent_is_error",
            "with no -pa given, init:get_argument(pa) is the atom `error`",
            "init:get_argument(pa)",
            "ok error",
        ),
        eval(
            "boot_extra/init_get_arguments_is_a_list",
            "init:get_arguments/0 returns a list",
            "is_list(init:get_arguments())",
            "ok true",
        ),
        // --- Emulator scheduling / limit constants of the booted system -------------------
        // The runtime's scheduling mode and the emulator's resource limits are the frozen
        // emulator constants a booted system reports; volatile counts are pinned by shape.
        eval(
            "boot_extra/system_info_multi_scheduling_is_enabled",
            "erlang:system_info(multi_scheduling) reports the enabled multi-scheduler mode",
            "erlang:system_info(multi_scheduling) =:= enabled",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_multi_scheduling_blockers_is_empty",
            "erlang:system_info(multi_scheduling_blockers) is the empty blocker list",
            "erlang:system_info(multi_scheduling_blockers) =:= []",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_break_ignored_is_false",
            "with no +B override, erlang:system_info(break_ignored) is `false`",
            "erlang:system_info(break_ignored) =:= false",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_thread_pool_size_is_positive",
            "erlang:system_info(thread_pool_size) is a positive integer (value is host-dependent)",
            "erlang:system_info(thread_pool_size) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_port_limit_is_positive",
            "erlang:system_info(port_limit) is a positive integer limit",
            "erlang:system_info(port_limit) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_process_limit_is_positive",
            "erlang:system_info(process_limit) is a positive integer limit",
            "erlang:system_info(process_limit) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_fullsweep_after_pair_shape",
            "erlang:system_info(fullsweep_after) is the {fullsweep_after,Words} pair",
            "begin F = erlang:system_info(fullsweep_after), is_tuple(F) andalso element(1, F) =:= fullsweep_after andalso is_integer(element(2, F)) end",
            "ok true",
        ),
        // --- The installation hierarchy the boot chain resolves ----------------------------
        // A booted system must resolve the *installation* it started from, not just its own
        // preloaded modules: the root directory, the library root, an application's directory
        // and the emulator's release constants. Only **relations** and **shapes** are pinned —
        // every path component is host-specific, so no path is ever asserted by value.
        eval(
            "boot_extra/system_info_system_version_is_non_empty",
            "erlang:system_info(system_version) is a non-empty string (the boot banner)",
            "length(erlang:system_info(system_version)) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_system_version_names_erlang_otp",
            "erlang:system_info(system_version) begins with the frozen product name \"Erlang/OTP\"",
            "lists:prefix(\"Erlang/OTP\", erlang:system_info(system_version))",
            "ok true",
        ),
        eval(
            "boot_extra/code_root_dir_is_absolute",
            "code:root_dir/0 is an absolute path (filename:absname/1 is a fixed point on it)",
            "filename:absname(code:root_dir()) =:= code:root_dir()",
            "ok true",
        ),
        eval(
            "boot_extra/code_lib_dir_0_is_non_empty_string",
            "code:lib_dir/0 is a non-empty string (the installation's library root)",
            "begin D = code:lib_dir(), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/code_lib_dir_0_prefixes_the_erts_application_dir",
            "the library root (code:lib_dir/0) is a path prefix of the erts application directory",
            "lists:prefix(code:lib_dir(), code:lib_dir(erts))",
            "ok true",
        ),
        eval(
            "boot_extra/code_lib_dir_0_differs_from_the_erts_application_dir",
            "the library root (code:lib_dir/0) is not the erts application directory itself",
            "code:lib_dir() =/= code:lib_dir(erts)",
            "ok true",
        ),
        eval(
            "boot_extra/code_root_dir_prefixes_the_library_root",
            "the installation root (code:root_dir/0) is a path prefix of the library root",
            "lists:prefix(code:root_dir(), code:lib_dir())",
            "ok true",
        ),
        eval(
            "boot_extra/code_root_dir_and_lib_dir_are_distinct",
            "the installation root and the library root are two different directories",
            "code:root_dir() =/= code:lib_dir()",
            "ok true",
        ),
        // The installation roots are *absolute* directories by construction — a relation that
        // survives on any host, unlike the path values themselves. The resolved object file of a
        // boot-loaded module must likewise live under the installation root.
        eval(
            "boot_extra/code_root_dir_pathtype_is_absolute",
            "the installation root (code:root_dir/0) is an absolute path (filename:pathtype/1)",
            "filename:pathtype(code:root_dir())",
            "ok absolute",
        ),
        eval(
            "boot_extra/code_lib_dir_pathtype_is_absolute",
            "the library root (code:lib_dir/0) is an absolute path (filename:pathtype/1)",
            "filename:pathtype(code:lib_dir())",
            "ok absolute",
        ),
        eval(
            "boot_extra/code_root_dir_prefixes_a_resolved_object_file",
            "a boot-loaded module's resolved object file lives under the installation root (code:root_dir/0)",
            "lists:prefix(code:root_dir(), code:which(lists))",
            "ok true",
        ),
        // --- The application resources the boot chain reads, by value shape ----------------
        // `application:get_key/2` reads the parsed `.app` resource of a started application;
        // its string-valued members are release-varying, so only their *non-emptiness* is
        // pinned. The collection- and manifest-valued keys are pinned by a positive length.
        eval(
            "boot_extra/application_get_key_kernel_description_is_non_empty_string",
            "the kernel application resource carries a non-empty `description` string",
            "begin {ok,D} = application:get_key(kernel, description), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_stdlib_description_is_non_empty_string",
            "the stdlib application resource carries a non-empty `description` string",
            "begin {ok,D} = application:get_key(stdlib, description), is_list(D) andalso length(D) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_stdlib_vsn_is_non_empty_string",
            "the stdlib application resource carries a non-empty `vsn` string",
            "begin {ok,Vs} = application:get_key(stdlib, vsn), is_list(Vs) andalso length(Vs) > 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_modules_is_non_empty",
            "application:get_key(kernel, modules) is a non-empty module manifest",
            "length(element(2, application:get_key(kernel, modules))) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/application_get_key_kernel_registered_is_non_empty",
            "application:get_key(kernel, registered) is a non-empty registered-name manifest",
            "length(element(2, application:get_key(kernel, registered))) > 0",
            "ok true",
        ),
        // --- The code server's load refusal ------------------------------------------------
        // `code:ensure_loaded/1` is how the boot chain pulls in a module that is not preloaded.
        // Its refusal reason for a module that exists nowhere is the frozen `nofile` atom.
        eval(
            "boot_extra/code_ensure_loaded_unknown_module_is_nofile",
            "code:ensure_loaded/1 reports the frozen {error,nofile} for a module that exists nowhere",
            "code:ensure_loaded(no_such_module_xyz)",
            "ok {error,nofile}",
        ),
        // --- `code:load_binary/3`'s shapes -------------------------------------------------
        // A memory load *prepares* the image and only then consults the code server's sticky
        // check, so the two refusals are distinguishable: a corrupt image is the frozen
        // `{error,badfile}` (the image never validates) while a valid image for a module that is
        // loaded from a sticky directory is `{error,sticky_directory}` (it validated first).
        // Both refusal paths also **log** asynchronously on the oracle's stdout, so this case
        // runs both runtimes with `-kernel logger_level none` (see `eval_with_argv`) and pins
        // the return value alone.
        eval_with_argv(
            "boot_extra/code_load_binary_shapes",
            "code:load_binary/3 refuses a valid image of a sticky-loaded module with the frozen \
             {error,sticky_directory} (the image validated first) and a corrupt image with \
             {error,badfile}",
            "begin {Mod, Bin, _} = code:get_object_code(lists), {code:load_binary(Mod, \
             \"lists.beam\", Bin), code:load_binary(mymod, \"/x/mymod.beam\", <<>>)} end",
            &["-kernel", "logger_level", "none"],
            "ok {{error,sticky_directory},{error,badfile}}",
        ),
        // --- The boot-time loaded set: the post-`{kernel_load_completed}` primLoad batches --
        // `bootstrap/bin/start.boot` carries three `{primLoad, …}` batches; `init` loads only the
        // one **before** `{kernel_load_completed}` eagerly outside embedded mode (the default),
        // so the modules of the later batches are not loaded at boot — and, because loading a
        // module is what interns its whole `Atoms` chunk, neither are the atoms only they carry:
        // `hello` rides `auth.beam` and `world` rides `net_adm.beam`, both post-marker names.
        eval(
            "boot_extra/prim_load_batches_after_kernel_load_completed_are_not_loaded",
            "the modules of the `{primLoad, …}` batches after `{kernel_load_completed}` (auth, \
             net_adm) are not loaded at boot, so the atom only their images carry (hello, from \
             auth.beam) does not exist",
            "begin Auth = code:is_loaded(auth), NetAdm = code:is_loaded(net_adm), Missing = try \
             erlang:list_to_existing_atom(\"hello\") catch error:Reason -> Reason end, \
             {Auth, NetAdm, Missing} end",
            "ok {false,false,badarg}",
        ),
        // --- The preloaded `atomics` runtime floor -----------------------------------------
        // `atomics` is preloaded (see the membership cases above); its observable floor is live
        // with no application started. The whole put/get family is pinned by value: the
        // operations are deterministic in a single process.
        eval(
            "boot_extra/atomics_put_get_round_trip",
            "the preloaded `atomics` module round-trips put/3 then get/2 on one slot",
            "begin A = atomics:new(1, []), atomics:put(A, 1, 5), atomics:get(A, 1) =:= 5 end",
            "ok true",
        ),
        eval(
            "boot_extra/atomics_add_get_returns_the_sum",
            "atomics:add_get/3 returns the sum it just stored",
            "begin A = atomics:new(1, []), atomics:add_get(A, 1, 3) =:= 3 end",
            "ok true",
        ),
        eval(
            "boot_extra/atomics_exchange_returns_the_previous_value",
            "atomics:exchange/3 returns the slot's previous value (0 for a fresh array)",
            "begin A = atomics:new(1, []), atomics:exchange(A, 1, 9) =:= 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/atomics_unsigned_round_trip",
            "an unsigned ({signed,false}) `atomics` array round-trips put/3 then get/2",
            "begin A = atomics:new(1, [{signed,false}]), atomics:put(A, 1, 5), atomics:get(A, 1) =:= 5 end",
            "ok true",
        ),
        // --- The preloaded `counters` runtime floor ----------------------------------------
        // `counters` is preloaded (see the membership cases above); its arithmetic floor is the
        // per-scheduler-free counter API the boot chain and `rpc`/`os_mon` rely on.
        eval(
            "boot_extra/counters_get_initial_is_zero",
            "a fresh `counters` array's first slot reads 0",
            "begin Cnt = counters:new(1, []), counters:get(Cnt, 1) =:= 0 end",
            "ok true",
        ),
        eval(
            "boot_extra/counters_add_round_trip",
            "counters:add/3 then get/2 returns the accumulated value",
            "begin Cnt = counters:new(1, []), counters:add(Cnt, 1, 7), counters:get(Cnt, 1) =:= 7 end",
            "ok true",
        ),
        eval(
            "boot_extra/counters_sub_returns_the_difference",
            "counters:sub/3 subtracts from the accumulated value",
            "begin Cnt = counters:new(1, []), counters:add(Cnt, 1, 7), counters:sub(Cnt, 1, 2), counters:get(Cnt, 1) =:= 5 end",
            "ok true",
        ),
        eval(
            "boot_extra/counters_put_sets_the_slot",
            "counters:put/3 overwrites the accumulated value",
            "begin Cnt = counters:new(1, []), counters:add(Cnt, 1, 7), counters:put(Cnt, 1, 3), counters:get(Cnt, 1) =:= 3 end",
            "ok true",
        ),
        eval(
            "boot_extra/counters_new_with_write_concurrency_round_trip",
            "a `counters` array created with the write_concurrency option still round-trips add/get",
            "begin Cnt = counters:new(1, [write_concurrency]), counters:add(Cnt, 1, 4), counters:get(Cnt, 1) =:= 4 end",
            "ok true",
        ),
        // --- The preloaded `persistent_term` runtime floor ---------------------------------
        // `persistent_term` is preloaded (see the membership cases above); erase/1 completes the
        // get/put/erase floor the boot chain's configuration handoff uses.
        eval(
            "boot_extra/persistent_term_erase_removes_the_key",
            "persistent_term:erase/1 removes a key so get/2 serves the default again",
            "begin persistent_term:put(boot_extra_pt_key, 1), persistent_term:erase(boot_extra_pt_key), persistent_term:get(boot_extra_pt_key, default) end",
            "ok default",
        ),
        eval(
            "boot_extra/persistent_term_round_trips_a_compound_term",
            "the preloaded `persistent_term` round-trips a compound (tuple) term unchanged",
            "begin persistent_term:put(boot_extra_pt_term, {a,b}), persistent_term:get(boot_extra_pt_term) end",
            "ok {a,b}",
        ),
        // --- Release constants a booted system must answer ---------------------------------
        // The emulator's release/limit constants the boot chain and `init` read at startup. The
        // release-dependent `compat_rel` number is pinned by **type** only (oracle OTP 29 vs the
        // rewrite's OTP 30), exactly like `otp_release`.
        eval(
            "boot_extra/system_info_compat_rel_is_an_integer",
            "erlang:system_info(compat_rel) is an integer (value is release-dependent)",
            "is_integer(erlang:system_info(compat_rel))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_ets_limit_is_positive",
            "erlang:system_info(ets_limit) is a positive integer limit",
            "erlang:system_info(ets_limit) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_scheduler_id_is_positive",
            "erlang:system_info(scheduler_id) is a positive scheduler index",
            "erlang:system_info(scheduler_id) > 0",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_min_bin_vheap_size_pair_shape",
            "erlang:system_info(min_bin_vheap_size) is the {min_bin_vheap_size,Words} pair",
            "begin M = erlang:system_info(min_bin_vheap_size), is_tuple(M) andalso element(1, M) =:= min_bin_vheap_size andalso is_integer(element(2, M)) end",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_os_type_shape",
            "erlang:system_info(os_type) is the {Family,Name} pair of atoms",
            "begin O = erlang:system_info(os_type), is_tuple(O) andalso is_atom(element(1, O)) andalso is_atom(element(2, O)) end",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_endian_is_an_atom",
            "erlang:system_info(endian) is an atom (little/big — the host's byte order)",
            "is_atom(erlang:system_info(endian))",
            "ok true",
        ),
        // --- The installation view, probed through the file system -------------------------
        // The boot chain reads the running installation out of the file system. These cases pin
        // the **relations** — `code:root_dir/0`, `code:lib_dir/0,2` and a resolved object file
        // name real directories/files — and never a path value.
        eval(
            "boot_extra/filelib_is_dir_of_the_installation_root",
            "the installation root code:root_dir/0 names an existing directory",
            "filelib:is_dir(code:root_dir())",
            "ok true",
        ),
        eval(
            "boot_extra/filelib_is_dir_of_the_library_root",
            "the library root code:lib_dir/0 names an existing directory",
            "filelib:is_dir(code:lib_dir())",
            "ok true",
        ),
        eval(
            "boot_extra/filelib_is_dir_of_the_erts_application_dir",
            "the erts application directory code:lib_dir(erts) names an existing directory",
            "filelib:is_dir(code:lib_dir(erts))",
            "ok true",
        ),
        eval(
            "boot_extra/filelib_is_dir_of_an_ebin_directory",
            "the code-path subdirectory code:lib_dir(kernel, ebin) names an existing directory",
            "filelib:is_dir(code:lib_dir(kernel, ebin))",
            "ok true",
        ),
        eval(
            "boot_extra/filelib_is_regular_of_the_library_root_is_false",
            "the library root code:lib_dir/0 is a directory, not a regular file",
            "not filelib:is_regular(code:lib_dir())",
            "ok true",
        ),
        eval(
            "boot_extra/filelib_is_regular_of_a_resolved_object_file",
            "the object-file path code:which/1 resolves names an existing regular file",
            "begin code:ensure_loaded(lists), filelib:is_regular(code:which(lists)) end",
            "ok true",
        ),
        eval(
            "boot_extra/code_object_path_suffix_is_the_objfile_extension",
            "a resolved object-file path ends in the frozen compiled-artifact extension",
            "begin code:ensure_loaded(lists), lists:suffix(code:objfile_extension(), code:which(lists)) end",
            "ok true",
        ),
        eval(
            "boot_extra/code_which_agrees_with_get_object_code_filename",
            "code:which/1 resolves the same file name code:get_object_code/1 read the module from",
            "begin code:ensure_loaded(lists), code:which(lists) =:= element(3, code:get_object_code(lists)) end",
            "ok true",
        ),
        eval(
            "boot_extra/code_is_sticky_erlang_is_true",
            "the preloaded `erlang` module is sticky (code:is_sticky/1 -> true)",
            "code:is_sticky(erlang)",
            "ok true",
        ),
        // The code server's own module and OTP's `application` controller module are boot-loaded
        // kernel modules (not preloaded), and the code server marks them sticky once the boot
        // script has loaded them — a boot-completion observable, not a path/pid.
        eval(
            "boot_extra/code_is_sticky_code_is_true",
            "the code server's own module (`code`) is sticky once the boot script has loaded it",
            "code:is_sticky(code)",
            "ok true",
        ),
        eval(
            "boot_extra/code_is_sticky_application_is_true",
            "OTP's application controller module (`application`) is sticky once the boot script has loaded it",
            "code:is_sticky(application)",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_every_module_can_be_ensure_loaded",
            "code:ensure_loaded/1 answers {module,Module} for every member of erlang:pre_loaded/0",
            "lists:all(fun(M) -> code:ensure_loaded(M) =:= {module,M} end, erlang:pre_loaded())",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_every_module_has_a_preloaded_object_path",
            "code:which/1 reports the atom `preloaded` for exactly the members of erlang:pre_loaded/0",
            "length([M || M <- erlang:pre_loaded(), code:which(M) =:= preloaded]) =:= 22",
            "ok true",
        ),
        // The two directions of the preloaded floor's membership: every module the code server
        // reports as `preloaded` is in `erlang:pre_loaded/0` and vice versa, and a module that is
        // merely on the code path (`lists`) is not preloaded.
        eval(
            "boot_extra/pre_loaded_is_exactly_the_preloaded_object_paths",
            "erlang:pre_loaded/0 is exactly the set of loaded modules the code server reports as `preloaded`",
            "lists:sort(erlang:pre_loaded()) =:= lists:sort([M || {M,_} <- code:all_loaded(), code:which(M) =:= preloaded])",
            "ok true",
        ),
        eval(
            "boot_extra/pre_loaded_omits_lists",
            "the code-path module `lists` is not a member of erlang:pre_loaded/0",
            "lists:member(lists, erlang:pre_loaded())",
            "ok false",
        ),
        // --- The kernel processes the boot chain registered --------------------------------
        // The boot script's `{kernelProcess, …}` directives leave these servers registered, so a
        // booted system resolves each name to a live pid. Each case asserts only the *pid-ness*
        // of the lookup — the pids are per-VM-start identities and are never pinned.
        eval(
            "boot_extra/whereis_file_server_2_is_registered",
            "the boot chain's file server is a live registered process (whereis(file_server_2) is a pid)",
            "is_pid(whereis(file_server_2))",
            "ok true",
        ),
        eval(
            "boot_extra/whereis_code_server_is_registered",
            "the boot chain's code server is a live registered process (whereis(code_server) is a pid)",
            "is_pid(whereis(code_server))",
            "ok true",
        ),
        eval(
            "boot_extra/whereis_kernel_sup_is_registered",
            "the boot chain's kernel supervisor is a live registered process (whereis(kernel_sup) is a pid)",
            "is_pid(whereis(kernel_sup))",
            "ok true",
        ),
        eval(
            "boot_extra/whereis_application_controller_is_registered",
            "the boot chain's application controller is a live registered process (whereis(application_controller) is a pid)",
            "is_pid(whereis(application_controller))",
            "ok true",
        ),
        eval(
            "boot_extra/whereis_logger_is_registered",
            "the boot chain's logger server is a live registered process (whereis(logger) is a pid)",
            "is_pid(whereis(logger))",
            "ok true",
        ),
        eval(
            "boot_extra/whereis_standard_error_is_registered",
            "the boot chain's standard-error I/O server is a live registered process (whereis(standard_error) is a pid)",
            "is_pid(whereis(standard_error))",
            "ok true",
        ),
        // `erl_prim_loader` is not a `{kernelProcess, …}` server — it is the loader's own
        // process, registered on every route that builds a runtime (the oracle registers its C
        // `erts_prim_loader` there) — but it too resolves to a live pid in a booted system.
        eval(
            "boot_extra/whereis_erl_prim_loader_is_registered",
            "the primitive loader is a live registered process (whereis(erl_prim_loader) is a pid)",
            "is_pid(whereis(erl_prim_loader))",
            "ok true",
        ),
        // --- The booted system's builtin/export floor --------------------------------------
        // `erlang:is_builtin/3` answers from the loader's own table (a BIF is not a module
        // export); `erlang:function_exported/3` answers from the loaded preloaded module.
        eval(
            "boot_extra/is_builtin_erlang_self_0",
            "erlang:is_builtin/3 reports erlang:self/0 as a BIF",
            "erlang:is_builtin(erlang, self, 0)",
            "ok true",
        ),
        eval(
            "boot_extra/is_builtin_erlang_is_builtin_3",
            "erlang:is_builtin/3 reports itself as a BIF",
            "erlang:is_builtin(erlang, is_builtin, 3)",
            "ok true",
        ),
        eval(
            "boot_extra/is_builtin_lists_sort_1_is_false",
            "erlang:is_builtin/3 is false for a real module export (lists:sort/1)",
            "erlang:is_builtin(lists, sort, 1) =:= false",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_persistent_term_get_1",
            "the preloaded `persistent_term` module exports get/1",
            "erlang:function_exported(persistent_term, get, 1)",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_atomics_new_2",
            "the preloaded `atomics` module exports new/2",
            "erlang:function_exported(atomics, new, 2)",
            "ok true",
        ),
        eval(
            "boot_extra/function_exported_erts_code_purger_purge_1",
            "the preloaded `erts_code_purger` module exports purge/1",
            "erlang:function_exported(erts_code_purger, purge, 1)",
            "ok true",
        ),
        // --- The preloaded `atomics`/`counters`/`persistent_term` runtime floor ------------
        eval(
            "boot_extra/atomics_sub_get_returns_the_difference",
            "atomics:sub_get/3 returns the difference it just stored (the signed floor)",
            "begin A = atomics:new(1, []), atomics:sub_get(A, 1, 2) =:= -2 end",
            "ok true",
        ),
        eval(
            "boot_extra/counters_atomics_backend_round_trip",
            "a `counters` array built on the `atomics` backend still round-trips add/get",
            "begin Cnt = counters:new(1, [atomics]), counters:add(Cnt, 1, 3), counters:get(Cnt, 1) =:= 3 end",
            "ok true",
        ),
        eval(
            "boot_extra/persistent_term_get_with_default_when_set",
            "persistent_term:get/2 serves the stored value (not the default) for a set key",
            "begin persistent_term:put(boot_extra_pt_set, 17), persistent_term:get(boot_extra_pt_set, default) end",
            "ok 17",
        ),
        eval(
            "boot_extra/persistent_term_erase_of_an_unset_key_is_false",
            "persistent_term:erase/1 reports `false` for a key that was never set",
            "persistent_term:erase(boot_extra_pt_never_set)",
            "ok false",
        ),
        // --- The kernel application's registered-name manifest -----------------------------
        eval(
            "boot_extra/application_get_key_kernel_registered_names_inet_db",
            "kernel's registered-name manifest names the boot chain's inet_db",
            "lists:member(inet_db, element(2, application:get_key(kernel, registered)))",
            "ok true",
        ),
        // --- Release-stable emulator observability -----------------------------------------
        eval(
            "boot_extra/system_info_garbage_collection_is_a_list",
            "erlang:system_info(garbage_collection) is the emulator's GC-parameter list",
            "is_list(erlang:system_info(garbage_collection))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_max_heap_size_is_a_map",
            "erlang:system_info(max_heap_size) is the emulator's max-heap-size map",
            "is_map(erlang:system_info(max_heap_size))",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_schedulers_online_at_most_schedulers",
            "the online scheduler count never exceeds the configured scheduler count",
            "erlang:system_info(schedulers_online) =< erlang:system_info(schedulers)",
            "ok true",
        ),
        eval(
            "boot_extra/system_info_endian_is_little_or_big",
            "erlang:system_info(endian) is one of the two frozen byte orders",
            "lists:member(erlang:system_info(endian), [little, big])",
            "ok true",
        ),
        // The emulator's own start timestamp is the booted system's own boot-time observable; its
        // *value* varies per VM start, so only its integer shape is pinned.
        eval(
            "boot_extra/system_info_start_time_is_integer",
            "erlang:system_info(start_time) is the emulator's boot timestamp (an integer, value never pinned)",
            "is_integer(erlang:system_info(start_time))",
            "ok true",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Contract, Probe};

    /// The one case here that drives the environment half of the CLI contract (A.6): the
    /// `-boot` init argument supplied through `ERL_AFLAGS`.
    const ENV_CASES: &[&str] = &["boot_extra/boot_flag_via_erl_aflags"];

    #[test]
    fn every_case_is_a_build_eval_case_in_the_boot_extra_group() {
        for case in group() {
            assert!(
                case.id.starts_with("boot_extra/"),
                "case {} is outside the boot_extra group",
                case.id
            );
            assert_eq!(case.contracts, &[Contract::Build], "case {}", case.id);
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            let probe = match case.kind {
                CaseKind::Eval { probe, .. } | CaseKind::EvalEnv { probe, .. } => probe,
                CaseKind::Module(_) | CaseKind::ModuleEnv { .. } | CaseKind::Command(_) => {
                    panic!("case {}: an A.9 boot case must be an eval case", case.id)
                }
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
        }
    }

    #[test]
    fn case_ids_are_unique_and_count_is_material() {
        use std::collections::HashSet;
        let cases = group();
        let mut seen = HashSet::new();
        for case in &cases {
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
        }
        assert!(
            cases.len() >= 20,
            "the expanded A.9 group must be material (got {})",
            cases.len()
        );
    }

    #[test]
    fn every_case_has_a_well_formed_ok_golden() {
        use std::collections::HashSet;
        let mut used: HashSet<&str> = HashSet::new();
        for case in group() {
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {} asserts stdout", case.id));
            assert!(
                golden.starts_with("ok "),
                "case {}: a Probe::Value golden is an `ok <value>` line, got {golden:?}",
                case.id
            );
            assert!(
                ALLOWED_GOLDENS.contains(&golden),
                "case {}: golden {golden:?} is outside the reviewed set",
                case.id
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            // No volatile rendering may be baked into a golden: no pid/ref/port (`<`), no
            // filesystem path (`/`), no map (`#{`).
            for unstable in ["<", "/", "#{"] {
                assert!(
                    !golden.contains(unstable),
                    "case {}: the golden pins an unstable rendering: {golden:?}",
                    case.id
                );
            }
            used.insert(golden);
        }
        for golden in ALLOWED_GOLDENS {
            assert!(
                used.contains(golden),
                "the reviewed golden set lists an unused golden: {golden:?}"
            );
        }
    }

    /// The one case here that carries an extra argv: `code_load_binary_shapes` silences the
    /// oracle's asynchronous refusal *report* on stdout (`beam_load.c`'s `corrupt file header` /
    /// "resides in sticky dir" log, which races with the probe's own output and is not part of
    /// `code:load_binary/3`'s return value) with the documented `-kernel logger_level none` kill
    /// switch — the same suppression the sibling oracle harnesses use
    /// (`crates/beam_process/tests/common/mod.rs`). Both runtimes still receive the **identical**
    /// argv (only the program path differs), like every other case in this group.
    const EXTRA_ARGV_CASES: &[(&str, &[&str])] = &[(
        "boot_extra/code_load_binary_shapes",
        &["-kernel", "logger_level", "none"],
    )];

    #[test]
    fn only_the_environment_case_carries_an_environment() {
        for case in group() {
            let has_env = !case.env().is_empty();
            assert_eq!(
                has_env,
                matches!(case.kind, CaseKind::EvalEnv { .. }),
                "case {}: env case/non-env case mismatch",
                case.id
            );
            // A case that does not pin the environment takes no extra argv either — except the
            // reviewed `EXTRA_ARGV_CASES` above, whose flag both runtimes receive identically.
            if case.env().is_empty() {
                let expected = EXTRA_ARGV_CASES
                    .iter()
                    .find(|(id, _)| *id == case.id)
                    .map(|(_, argv)| *argv)
                    .unwrap_or(&[]);
                assert_eq!(
                    case.extra_argv(),
                    expected,
                    "case {}: unexpected extra argv",
                    case.id
                );
            }
        }
        // A reviewed exception that no longer names a case here would otherwise go unnoticed.
        for (id, argv) in EXTRA_ARGV_CASES {
            let case = group()
                .into_iter()
                .find(|c| c.id == *id)
                .unwrap_or_else(|| panic!("the extra-argv set lists a missing case: {id}"));
            assert_eq!(case.extra_argv(), *argv, "case {id}");
        }
        // The environment case pins `ERL_AFLAGS` (the documented variable) and nothing else.
        for id in ENV_CASES {
            let case = group()
                .into_iter()
                .find(|c| c.id == *id)
                .unwrap_or_else(|| panic!("case {id} is missing"));
            assert_eq!(case.env(), &[("ERL_AFLAGS", "-boot start")], "case {id}");
            assert!(
                case.expr()
                    .is_some_and(|e| e.contains("init:get_argument(boot)")),
                "case {id} must observe the `boot` init argument"
            );
        }
    }

    /// Every golden this group may assert. The list is explicit (rather than a set of loose shape
    /// rules) so that a new case cannot slip a volatile rendering past review, and the test also
    /// asserts that no entry here is unused. Volatile values are observed by shape only, so
    /// whatever a case asserts is either a frozen constant or a boolean/tuple-shaped predicate.
    const ALLOWED_GOLDENS: &[&str] = &[
        "ok true",
        "ok false",
        "ok []",
        "ok error",
        "ok undefined",
        "ok preloaded",
        "ok non_existing",
        "ok default",
        "ok lists",
        "ok init",
        "ok {ok,kernel}",
        "ok {ok,stdlib}",
        "ok {file,preloaded}",
        "ok {module,lists}",
        "ok {error,bad_name}",
        "ok {error,nofile}",
        "ok {{error,sticky_directory},{error,badfile}}",
        "ok {false,false,badarg}",
        "ok {starting,started}",
        "ok {a,b}",
        "ok absolute",
        "ok 17",
    ];

    /// The keys whose *value* varies per VM start, per host or per OTP release. A case may observe
    /// their **shape** (`is_list/1`, `> 0`, `is_integer/1`), but must never compare the value
    /// itself — the oracle is OTP 29 while the rewrite targets OTP 30.
    const VOLATILE_KEYS: &[&str] = &[
        "process_count",
        "wall_clock",
        "unique_integer",
        "otp_release",
        "system_version",
        "logical_processors",
        "creation",
    ];

    #[test]
    fn no_expression_pins_a_volatile_value() {
        for case in group() {
            let expr = case.expr().expect("every case is an eval case");
            for key in VOLATILE_KEYS {
                assert!(
                    !expr.contains(&format!("{key}) =:=")) && !expr.contains(&format!("{key}) ==")),
                    "case {}: the expression pins the volatile `{key}` value: {expr:?}",
                    case.id
                );
            }
        }
    }
}
