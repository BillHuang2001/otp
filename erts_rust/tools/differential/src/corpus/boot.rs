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
//! `preloaded`, `{error,bad_name}`, `".beam"`) are asserted exactly.
//!
//! The last two cases deliberately record a **capability gap**: `init:get_status/0`
//! (the boot-status tuple) and `code:root_dir/0` (the installation root) are not reached
//! by the Rust runtime's `-eval` subset yet, so it signals `unsupported` (exit `3`) — an
//! honest *gap*, never a fabricated pass and never a `mismatch`.

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
        // --- Capability gaps, recorded honestly as `unsupported` (never a mismatch) -----
        Case {
            id: "boot/init_get_status_is_tuple",
            description: "init:get_status/0 is the {State,Started} boot-status tuple (Rust: unsupported)",
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
