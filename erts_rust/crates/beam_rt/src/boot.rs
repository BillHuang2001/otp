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

//! The boot surface: the preloaded-module registry skeleton (A.7), the atom seed and
//! the version banner.
//!
//! Compatibility contract **A.7** requires the *OTP application contracts* that the
//! runtime itself provides: the preloaded modules (the modules that are loaded before
//! any other code, straight out of the emulator image) and the `erlang`/`erts_internal`/
//! `prim_*` surfaces. The **name registry** of the standard preloaded set
//! ([`PRELOADED_MODULE_NAMES`], `erts/preloaded/src/*.erl` of the legacy tree) and the
//! **boot sequence** hook — [`crate::runtime::Runtime::boot`] — are here; the boot hook
//! now also loads the matching `.beam` files into the shared module cache for real, see
//! the `preloaded` submodule.
//!
//! The core-atom seed is the second half: [`Runtime`](crate::runtime::Runtime) owns a
//! **global atom table** used by `system_info/1` and the `~p` printer, and every atom
//! those surfaces can emit must be interned there. A BIF that needs an atom of a
//! *loaded module* uses that module's own atom table instead (see the crate
//! `CONTEXT.md`, "Atom-table bridge").

use beam_common::{ERTS_VSN, ERTS_VERSION, OTP_VERSION};
use beam_term::AtomTable;

pub mod bootscript;
/// The boot script's bounded scheduler drives (`drive_scheduler`/`apply_boot_call`).
mod drive;
mod preloaded;

pub(crate) use preloaded::load_preloaded;

/// The standard preloaded modules of this release, in `erlang:pre_loaded/0`'s order.
///
/// The set **and the order** mirror the oracle (OTP 29/erts-17.1,
/// `erl -noshell -eval 'io:format("~w~n",[erlang:pre_loaded()]),halt(0).'`), which lists
/// the modules that live in the emulator image itself — available before any `.beam` file
/// is read. `erts_dirty_process_signal_handler`, `erts_trace_cleaner` and
/// `erts_literal_area_collector` are internal support modules of the same set.
///
/// The order is the oracle's, so `erlang:pre_loaded/0`,
/// [`PreloadedModules::names`] and `code:all_loaded/0` (which lists the preloaded names
/// first) reproduce it verbatim; nothing here may be reordered without re-deriving the
/// list from the oracle.
pub const PRELOADED_MODULE_NAMES: &[&str] = &[
    "persistent_term",
    "counters",
    "atomics",
    "erts_dirty_process_signal_handler",
    "erts_trace_cleaner",
    "erts_literal_area_collector",
    "erl_tracer",
    "erts_internal",
    "erlang",
    "erl_prim_loader",
    "prim_zip",
    "prim_net",
    "prim_socket",
    "socket_registry",
    "zlib",
    "prim_file",
    "prim_inet",
    "prim_eval",
    "prim_buffer",
    "init",
    "erl_init",
    "erts_code_purger",
];

/// The core atoms the runtime's global atom table is seeded with at construction.
///
/// This is the *core* subset of the seed: [`seed_core_atoms`] also interns the whole
/// `system_info/1` atom vocabulary ([`crate::info::INFO_ATOMS`]) and the
/// `beam_process` signal names, so this list is deliberately not exhaustive on its own.
///
/// Five groups:
///
/// * the atoms `system_info/1` can materialize — the `os_type` pair of both platforms
///   and the boolean atoms (`unix`, `linux`, `win32`, `nt`, `true`, `false`);
/// * a small **documented core set** covering the runtime's own vocabulary and the
///   signal machinery of `beam_process` (`ok`, `error`, `undefined`, `EXIT`, `DOWN`,
///   `process`, `normal`, `kill`, `killed`), plus the module names of the boot surface;
/// * the atoms the runtime-native `code:*` floor ([`crate::native`]) answers with —
///   `preloaded`/`non_existing` (`code:which/1`, `code:all_loaded/0`) and `bad_name`
///   (the `{error, bad_name}` of `code:lib_dir/1`);
/// * the **exception reason/class** atoms the engine's `func_info` and
///   `raise_bif_error` paths resolve from the *running module's* atom table at raise
///   time: the bare reason `function_clause` (C: `i_func_info_IaaI` sets
///   `EXC_FUNCTION_CLAUSE`; the call arguments ride in the stacktrace, not the reason)
///   and the two remaining class names `exit`/`throw` (`error` is already in the core
///   set above). Seeding them here is what lets a module that never mentions the name
///   itself still raise the reason the oracle raises;
/// * the C runtime's **globally interned** vocabulary: the `error_atom` table
///   (`erts/emulator/beam/beam_common.c:382-406`), the stacktrace auxiliaries
///   `file`/`line`, the node-name atom `nonode@nohost` (the local-node identity
///   [`crate::context`] installs so `term_to_binary/1` can encode an internal
///   pid/port id) and the `bm`/`ac` compiled-pattern tags of `beam_bifs::binary`.
///   Real ERTS interns these up front; a raise, a catch and a printer can therefore
///   name them even when the raising module never wrote the name.
///
/// Everything after the first three groups is **appended last** on purpose: every index
/// assigned before it must stay stable (module code and the positional-atom tests depend
/// on the existing prefix), so the groups may only ever be extended at the end.
///
/// This is deliberately **not** the full `atom.names` ordering of the legacy emulator:
/// positional atom indices for preloaded code are a documented Milestone-1 gap.
pub const CORE_ATOMS: &[&str] = &[
    // `system_info/1` values.
    "unix",
    "linux",
    "win32",
    "nt",
    "true",
    "false",
    // Runtime vocabulary.
    "ok",
    "error",
    "undefined",
    "erlang",
    "init",
    "erts_internal",
    "erl_prim_loader",
    "kernel",
    "stdlib",
    // The runtime-native `code:*` floor (`native.rs`).
    "preloaded",
    "non_existing",
    "bad_name",
    // `beam_process` signal vocabulary (`SignalAtoms::from_table`).
    "EXIT",
    "DOWN",
    "process",
    "normal",
    "kill",
    "killed",
    // Exception reason/class atoms resolved from the *module* table at raise time
    // (the engine's `func_info`/`raise_bif_error` paths), plus the whole global
    // reason vocabulary of the C runtime's `error_atom` table
    // (`erts/emulator/beam/beam_common.c:382-406`). Appended last so every index
    // above is unchanged.
    "function_clause",
    "exit",
    "throw",
    // The `error_atom` table.
    "internal_error",
    "badarg",
    "badarith",
    "badmatch",
    "case_clause",
    "if_clause",
    "undef",
    "badfun",
    "badarity",
    "timeout_value",
    "noproc",
    "notalive",
    "system_limit",
    "try_clause",
    "notsup",
    "badmap",
    "badkey",
    "badrecord",
    "badfield",
    "novalue",
    // Stacktrace auxiliary atoms: the `file`/`line` of a `{M,F,A,Location}` frame.
    "file",
    "line",
    // The runtime's node-name atom — the local-node identity
    // [`crate::context`] installs so `term_to_binary/1` can encode an internal pid
    // as `NEW_PID_EXT` — and the two compiled-pattern tag atoms of the `binary`
    // module (`{bm, Ref}`/`{ac, Ref}`, `beam_bifs::binary`).
    "nonode@nohost",
    "bm",
    "ac",
    // The `get_module_info/1,2` item vocabulary and the compiler keys an `Attr`/`CInf`
    // chunk's decoded term carries (`vsn`, `version`, `options`, `source`, …). The module
    // seam resolves every atom **by name** into this table (it may not intern), so a name
    // absent here would make a legitimately renderable item an honest capability gap.
    // Appended last, like every group below the first three.
    "vsn",
    "version",
    "options",
    "source",
    "count",
    "memory",
    "module_info",
    "nifs",
    "native",
    "native_addresses",
    "records",
    "exports",
    "functions",
    "attributes",
    "compile",
    "md5",
    "module",
    // The tag atom the `RuntimeHooks` timer family emits
    // (`erlang:start_timer/3,4` builds `{timeout, TRef, Msg}`). The bridge may not intern
    // — the shared table is borrowed immutably for the whole BIF call — so the name must
    // be seeded. Appended after every existing group, as the append-only rule requires.
    "timeout",
];

/// The preloaded-module registry skeleton (contract A.7).
///
/// A name set, in `erlang:pre_loaded/0`'s order; [`is_preloaded`](PreloadedModules::is_preloaded)
/// is the question the loader/boot code asks, and [`names`](PreloadedModules::names)
/// iterates that order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloadedModules {
    names: Vec<&'static str>,
}

impl Default for PreloadedModules {
    /// The standard set of [`PRELOADED_MODULE_NAMES`].
    fn default() -> Self {
        PreloadedModules::standard()
    }
}

impl PreloadedModules {
    /// The standard preloaded set of this release ([`PRELOADED_MODULE_NAMES`]).
    pub fn standard() -> PreloadedModules {
        PreloadedModules {
            names: PRELOADED_MODULE_NAMES.to_vec(),
        }
    }

    /// Whether `name` is a preloaded module.
    pub fn is_preloaded(&self, name: &str) -> bool {
        self.names.contains(&name)
    }

    /// The preloaded module names, in `erlang:pre_loaded/0`'s order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.names.iter().copied()
    }

    /// The number of preloaded modules.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the registry is empty (never true for the standard set).
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Interns [`CORE_ATOMS`], the whole `system_info/1` atom vocabulary
/// ([`crate::info::INFO_ATOMS`]) and the signal vocabulary of `beam_process`.
///
/// Idempotent — [`AtomTable::intern`] returns the existing index for a name already
/// present — so it is safe to call on a table that already has entries.
pub fn seed_core_atoms(atoms: &mut AtomTable) {
    for name in CORE_ATOMS {
        atoms.intern(name);
    }
    // The introspection surface ([`crate::info`]) materializes its atom-valued keys
    // through this table, so every name it can emit must be present. `INFO_ATOMS` is the
    // single source of truth for that vocabulary (the platform/boolean group overlaps
    // `CORE_ATOMS`, and interning is idempotent), so derive the seed from it rather than
    // repeating the names here.
    for name in crate::info::INFO_ATOMS {
        atoms.intern(name);
    }
    // The port surface ([`crate::port`]) resolves the atoms `port_info/1,2` can emit
    // through this same table, so the whole vocabulary must be present for the same
    // reason: a name that is missing would make the hook report `TermNotFound` instead of
    // the item. `PORT_INFO_ATOMS` is that vocabulary's single source of truth.
    for name in crate::port::PORT_INFO_ATOMS {
        atoms.intern(name);
    }    // The signal machinery interns its own names through this helper; doing it here
    // keeps the boot sequence in one place.
    beam_process::SignalAtoms::from_table(atoms);
}

/// The version banner printed by `-version`/`--version` (contract A.6).
///
/// Names both the OTP release ([`OTP_VERSION`], `30.0-rc0`) and the ERTS version
/// ([`ERTS_VSN`], `erts-17.1`), which is what the CLI contract pins.
pub fn boot_banner() -> String {
    format!("Erlang/OTP {OTP_VERSION} ({ERTS_VSN}) [source] [64-bit]")
}

/// The `erlang:system_info(version)` value of this build ([`ERTS_VERSION`], `"17.1"`).
pub fn erts_version() -> &'static str {
    ERTS_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_registry_has_the_expected_members() {
        let preloaded = PreloadedModules::standard();
        assert_eq!(preloaded, PreloadedModules::default());
        assert_eq!(preloaded.len(), PRELOADED_MODULE_NAMES.len());
        assert!(!preloaded.is_empty());
        for name in [
            "erlang",
            "init",
            "erts_internal",
            "erl_prim_loader",
            "prim_file",
        ] {
            assert!(preloaded.is_preloaded(name), "{name} must be preloaded");
        }
        assert!(!preloaded.is_preloaded("lists"));
        assert!(!preloaded.is_preloaded("slice"));
    }

    #[test]
    fn names_iterates_in_pre_loaded_order() {
        let preloaded = PreloadedModules::standard();
        let names: Vec<&str> = preloaded.names().collect();
        assert_eq!(names.len(), preloaded.len());
        assert_eq!(names, PRELOADED_MODULE_NAMES);
        // The exact oracle order (`erlang:pre_loaded/0`), head and tail.
        assert_eq!(names[0], "persistent_term");
        assert_eq!(names[1], "counters");
        assert_eq!(names[8], "erlang");
        assert_eq!(names[names.len() - 1], "erts_code_purger");
    }

    #[test]
    fn the_pre_loaded_list_matches_the_oracle_exactly() {
        // Pinned against `erl -noshell -eval 'io:format("~w~n",[erlang:pre_loaded()]),halt(0).'`
        // (OTP 29/erts-17.1): set **and** order.
        let expected = [
            "persistent_term",
            "counters",
            "atomics",
            "erts_dirty_process_signal_handler",
            "erts_trace_cleaner",
            "erts_literal_area_collector",
            "erl_tracer",
            "erts_internal",
            "erlang",
            "erl_prim_loader",
            "prim_zip",
            "prim_net",
            "prim_socket",
            "socket_registry",
            "zlib",
            "prim_file",
            "prim_inet",
            "prim_eval",
            "prim_buffer",
            "init",
            "erl_init",
            "erts_code_purger",
        ];
        assert_eq!(PRELOADED_MODULE_NAMES, expected);
    }

    #[test]
    fn core_atoms_seed_is_idempotent_and_complete() {
        let mut atoms = AtomTable::new();
        seed_core_atoms(&mut atoms);
        let after_first = atoms.len();
        seed_core_atoms(&mut atoms);
        assert_eq!(atoms.len(), after_first, "seeding is idempotent");
        for name in CORE_ATOMS {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
        for name in ["EXIT", "DOWN", "normal", "kill", "killed", "process"] {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
        // The runtime-native `code:*` floor's answer atoms.
        for name in ["preloaded", "non_existing", "bad_name"] {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
    }

    #[test]
    fn seed_core_atoms_interns_the_whole_info_vocabulary() {
        // The introspection surface (`crate::info`) resolves an `InfoValue::Atom` through
        // this table and reports `TermNotFound` for a name that is absent, so the seed
        // must contain every name `system_info/1` can emit. `INFO_ATOMS` is that list and
        // the seed is derived from it, so deriving and asserting cannot drift apart; the
        // flavor atom (`jit` for the JIT emulator, `emu` for the interpreter) is the one
        // that used to fall through the seed.
        let mut atoms = AtomTable::new();
        seed_core_atoms(&mut atoms);
        for name in crate::info::INFO_ATOMS {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
        for name in ["jit", "emu", "true", "false", "unix", "linux", "win32", "nt"] {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
    }

    #[test]
    fn seed_core_atoms_interns_the_whole_port_vocabulary() {
        // The port provider (`crate::port`) resolves every `port_info/1,2` item atom
        // through this table and reports `TermNotFound` for a name that is absent, so the
        // boot seed must contain the whole `PORT_INFO_ATOMS` vocabulary.
        let mut atoms = AtomTable::new();
        seed_core_atoms(&mut atoms);
        for name in crate::port::PORT_INFO_ATOMS {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
        for name in ["name", "id", "connected", "os_pid", "links", "monitors", "input", "output"]
        {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
    }
    #[test]
    fn seed_core_atoms_interns_the_exception_reason_and_class_atoms() {
        // The engine's `func_info` arm resolves the failed-clause reason and the exception
        // CLASS atom from the *running module's* atom table at raise time. A module that
        // never mentions either name cannot name them itself (`function_clause_fixture.beam`
        // holds only `function_clause_fixture`, `f`, `a`, `ok`, `module_info`, `erlang`,
        // `get_module_info`), so the boot seed is the only source: without `function_clause`
        // a failed function head was a typed `unsupported` capability gap instead of the
        // oracle's `error:function_clause`.
        //
        // The same is true of the whole C `error_atom` table (`beam_common.c:382-406`),
        // which real ERTS interns up front, plus the stacktrace auxiliaries `file`/`line`
        // and the node-name atom `nonode@nohost` (`term_to_binary/1` of an internal pid
        // needs it to emit `NEW_PID_EXT`) and the `bm`/`ac` pattern tags.
        let tail = [
            "function_clause",
            "exit",
            "throw",
            "internal_error",
            "badarg",
            "badarith",
            "badmatch",
            "case_clause",
            "if_clause",
            "undef",
            "badfun",
            "badarity",
            "timeout_value",
            "noproc",
            "notalive",
            "system_limit",
            "try_clause",
            "notsup",
            "badmap",
            "badkey",
            "badrecord",
            "badfield",
            "novalue",
            "file",
            "line",
            "nonode@nohost",
            "bm",
            "ac",
            // The `get_module_info/1,2` item names and the compiler keys an `Attr`/`CInf`
            // chunk carries, appended after the reason/class group (see `CORE_ATOMS`).
            "vsn",
            "version",
            "options",
            "source",
            "count",
            "memory",
            "module_info",
            "nifs",
            "native",
            "native_addresses",
            "records",
            "exports",
            "functions",
            "attributes",
            "compile",
            "md5",
            "module",
            "timeout",
        ];
        let mut atoms = AtomTable::new();
        seed_core_atoms(&mut atoms);
        for name in tail {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
        // `error` is the class of every raise the two paths produce; it is part of the
        // documented core set.
        assert!(atoms.lookup("error").is_some(), "error must be seeded");
        // The group is **appended**: every index the seed assigned before it is unchanged
        // (module code and the positional-atom assumptions depend on the prefix), so a
        // table seeded with the prefix alone must resolve those names identically.
        let split = CORE_ATOMS.len() - tail.len();
        assert_eq!(
            &CORE_ATOMS[split..],
            &tail,
            "the appended group must be the last entries, in this order"
        );
        let mut prefix = AtomTable::new();
        for name in &CORE_ATOMS[..split] {
            prefix.intern(name);
        }
        for name in &CORE_ATOMS[..split] {
            assert_eq!(
                atoms.lookup(name),
                prefix.lookup(name),
                "{name} must keep the index it had before the appended group"
            );
        }
    }
    #[test]
    fn seed_core_atoms_interns_the_global_reason_vocabulary() {
        // The engine resolves a raise's reason atom from the *running module's* atom
        // table and never substitutes a different atom, so a module that does not mention
        // `badmatch`/`case_clause`/… itself can only raise it because the boot seed holds
        // the name — exactly what real ERTS's global `error_atom` table provides. The
        // `beam_bifs` `records:get_definition/2` path and `term_to_binary/1`'s internal
        // pid/port encoding depend on the same seed (`nonode@nohost`).
        let mut atoms = AtomTable::new();
        seed_core_atoms(&mut atoms);
        for name in [
            "internal_error",
            "badarg",
            "badarith",
            "badmatch",
            "case_clause",
            "if_clause",
            "undef",
            "badfun",
            "badarity",
            "timeout_value",
            "noproc",
            "notalive",
            "system_limit",
            "try_clause",
            "notsup",
            "badmap",
            "badkey",
            "badrecord",
            "badfield",
            "novalue",
            "file",
            "line",
            "nonode@nohost",
            "bm",
            "ac",
        ] {
            assert!(atoms.lookup(name).is_some(), "{name} must be seeded");
        }
    }
    #[test]
    fn banner_names_otp_and_erts() {
        let banner = boot_banner();
        assert!(banner.contains(OTP_VERSION), "{banner}");
        assert!(banner.contains("30.0-rc0"), "{banner}");
        assert!(banner.contains(ERTS_VSN), "{banner}");
        assert!(banner.contains("erts-17.1"), "{banner}");
        assert_eq!(erts_version(), "17.1");
    }
}
