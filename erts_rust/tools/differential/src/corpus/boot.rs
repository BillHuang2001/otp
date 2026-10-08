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
//! Finally the group pins the **boot-path prerequisite primitives** — the booted system's
//! `erlang:pre_loaded/0` set, the applications the boot script has started
//! (`which_applications/0`, `loaded_applications/0`), the preloaded `init` as seen by the
//! code server, kernel's `error_logger`/`vsn` application entries, and the boot script's own
//! `register/2` + `whereis/1` and group-leader-inheritance kernel-process primitives — plus
//! the `-boot` init argument (A.6) and the **stopping point** of the real boot chain
//! (`proc_lib:start_link/4`'s cross-module `proc_lib:sync_start/2` wait, folded into the
//! engine's typed `blocked` gap ⇒ the one honest `unsupported` case here). Every other case
//! below `agree`s.
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
}