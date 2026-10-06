//! The boot surface: the preloaded-module registry skeleton (A.7), the atom seed and
//! the version banner.
//!
//! Compatibility contract **A.7** requires the *OTP application contracts* that the
//! runtime itself provides: the preloaded modules (the modules that are loaded before
//! any other code, straight out of the emulator image) and the `erlang`/`erts_internal`/
//! `prim_*` surfaces. Milestone-1 ships the **name registry** of the standard preloaded
//! set (`erts/preloaded/src/*.erl` of the legacy tree) and the **boot order** hook —
//! [`crate::runtime::Runtime::boot`] — but does *not* yet load the preloaded `.beam`s
//! (see the crate `CONTEXT.md`, Known Issues).
//!
//! The core-atom seed is the second half: [`Runtime`](crate::runtime::Runtime) owns a
//! **global atom table** used by `system_info/1` and the `~p` printer, and every atom
//! those surfaces can emit must be interned there. A BIF that needs an atom of a
//! *loaded module* uses that module's own atom table instead (see the crate
//! `CONTEXT.md`, "Atom-table bridge").

use beam_common::{ERTS_VSN, ERTS_VERSION, OTP_VERSION};
use beam_term::AtomTable;

/// The standard preloaded modules of this release, in the emulator's boot order.
///
/// The list mirrors `erts/preloaded/src/` of the legacy tree: the modules that exist in
/// the emulator image itself and are therefore available before any `.beam` file is
/// read. `erts_dirty_process_signal_handler` and `erts_literal_area_collector` are
/// internal support modules of the same set.
pub const PRELOADED_MODULE_NAMES: &[&str] = &[
    "erlang",
    "erts_internal",
    "erl_prim_loader",
    "init",
    "erl_init",
    "erts_code_purger",
    "erts_dirty_process_signal_handler",
    "erts_literal_area_collector",
    "erts_trace_cleaner",
    "atomics",
    "counters",
    "erl_tracer",
    "persistent_term",
    "prim_buffer",
    "prim_eval",
    "prim_file",
    "prim_inet",
    "prim_net",
    "prim_socket",
    "prim_zip",
    "socket_registry",
    "zlib",
];

/// The atoms the runtime's global atom table is seeded with at construction.
///
/// Three groups:
///
/// * the atoms `system_info/1` can materialize — the `os_type` pair of both platforms
///   and the boolean atoms (`unix`, `linux`, `win32`, `nt`, `true`, `false`);
/// * a small **documented core set** covering the runtime's own vocabulary and the
///   signal machinery of `beam_process` (`ok`, `error`, `undefined`, `EXIT`, `DOWN`,
///   `process`, `normal`, `kill`, `killed`), plus the module names of the boot surface;
/// * the atoms the runtime-native `code:*` floor ([`crate::native`]) answers with —
///   `preloaded`/`non_existing` (`code:which/1`, `code:all_loaded/0`) and `bad_name`
///   (the `{error, bad_name}` of `code:lib_dir/1`).
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
];

/// The preloaded-module registry skeleton (contract A.7).
///
/// A name set, in boot order; [`is_preloaded`](PreloadedModules::is_preloaded) is the
/// question the loader/boot code asks, and [`names`](PreloadedModules::names) iterates
/// the boot order.
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

    /// The preloaded module names, in boot order.
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

/// Interns [`CORE_ATOMS`] (and the signal vocabulary of `beam_process`).
///
/// Idempotent — [`AtomTable::intern`] returns the existing index for a name already
/// present — so it is safe to call on a table that already has entries.
pub fn seed_core_atoms(atoms: &mut AtomTable) {
    for name in CORE_ATOMS {
        atoms.intern(name);
    }
    // The signal machinery interns its own names through this helper; doing it here
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
    fn names_iterates_in_boot_order() {
        let preloaded = PreloadedModules::standard();
        let names: Vec<&str> = preloaded.names().collect();
        assert_eq!(names.len(), preloaded.len());
        assert_eq!(names[0], "erlang");
        assert_eq!(names[1], "erts_internal");
        assert_eq!(names, PRELOADED_MODULE_NAMES);
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
    fn banner_names_otp_and_erts() {
        let banner = boot_banner();
        assert!(banner.contains(OTP_VERSION), "{banner}");
        assert!(banner.contains("30.0-rc0"), "{banner}");
        assert!(banner.contains(ERTS_VSN), "{banner}");
        assert!(banner.contains("erts-17.1"), "{banner}");
        assert_eq!(erts_version(), "17.1");
    }
}
