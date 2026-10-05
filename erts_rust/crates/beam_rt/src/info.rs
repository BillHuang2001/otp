//! `system_info/1` (contract A.8): the introspection surface of the runtime.
//!
//! The source of truth is [`system_info_value`], a **heap-free** function returning an
//! [`InfoValue`]; [`InfoValue::to_eterm`] materializes it on a heap exactly the way the
//! oracle does. Splitting the two makes every key unit-testable without allocating and
//! keeps the materialization in one place.
//!
//! Strings are Erlang **strings** — lists of byte-sized small integers — matching what
//! `erlang:system_info/1` returns in the oracle (erts-17.1): `version` is `"17.1"`,
//! i.e. `[49,55,46,49]`.
//!
//! # The supported key set
//!
//! [`SUPPORTED_KEYS`] is the complete list; **any other key is `badarg`**, which is
//! what the oracle raises (`erlang:system_info(nonsense)` ⇒ `error:badarg`).
//! `erts_version` is a **non-standard alias** of `version` kept for convenience
//! (real ERTS raises `badarg` for it) — see the crate `CONTEXT.md`, Known Issues.

use beam_abi::{ERL_NIF_MAJOR_VERSION, ERL_NIF_MINOR_VERSION};
use beam_alloc::Heap;
use beam_common::ERTS_VERSION;
use beam_common::{BeamError, Result};
use beam_term::{make_atom, try_make_small, AtomTable, Eterm, NIL};

use crate::error;
use crate::runtime::Runtime;

/// The keys [`system_info_value`] answers, in the order of the crate `CONTEXT.md`.
pub const SUPPORTED_KEYS: &[&str] = &[
    "version",
    "erts_version",
    "otp_release",
    "system_architecture",
    "system_version",
    "nif_version",
    "wordsize",
    "logical_processors",
    "process_count",
    "os_type",
    "threads",
];

/// A `system_info/1` value, independent of any heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoValue {
    /// A small integer (materialized as a small-int immediate).
    Int(i64),
    /// A string (materialized as a list of byte-sized small integers, as the oracle does).
    Str(String),
    /// An atom, by name (materialized through the runtime's global atom table).
    Atom(&'static str),
    /// A boolean (materialized as the atom `true`/`false`).
    Bool(bool),
    /// A two-element tuple of atoms, by name.
    Tuple(&'static str, &'static str),
}

impl InfoValue {
    /// Materializes this value as a term on `heap`, resolving atoms through `atoms`.
    ///
    /// # Errors
    ///
    /// * [`BeamError::SystemLimit`] when an [`InfoValue::Int`] does not fit a 60-bit
    ///   small integer (impossible for the keys of this crate);
    /// * [`BeamError::TermNotFound`] when an atom this value names is not interned in
    ///   `atoms` (the runtime's boot seed guarantees the boot atoms are);
    /// * the heap's allocation failure for a string or a tuple.
    pub fn to_eterm(&self, heap: &mut Heap, atoms: &AtomTable) -> Result<Eterm> {
        match self {
            InfoValue::Int(value) => try_make_small(*value).map_err(|_| BeamError::SystemLimit),
            InfoValue::Str(text) => {
                let mut list = NIL;
                for byte in text.bytes().rev() {
                    list = heap.cons(byte_small(byte)?, list)?;
                }
                Ok(list)
            }
            InfoValue::Atom(name) => atom_term(atoms, name),
            InfoValue::Bool(value) => atom_term(atoms, if *value { "true" } else { "false" }),
            InfoValue::Tuple(first, second) => {
                let first = atom_term(atoms, first)?;
                let second = atom_term(atoms, second)?;
                heap.tuple(&[first, second])
            }
        }
    }
}

/// One byte as a small-int immediate (an Erlang string element; always in range).
fn byte_small(byte: u8) -> Result<Eterm> {
    try_make_small(i64::from(byte)).map_err(|_| BeamError::SystemLimit)
}

/// An interned atom as a term.
fn atom_term(atoms: &AtomTable, name: &str) -> Result<Eterm> {
    atoms
        .lookup(name)
        .map(make_atom)
        .ok_or(BeamError::TermNotFound)
}

/// Computes the `system_info/1` value of `key` against a runtime.
///
/// # Errors
///
/// [`BeamError::BadArg`] for any key outside [`SUPPORTED_KEYS`] — the oracle's reason.
pub fn system_info_value(key: &str, rt: &Runtime) -> Result<InfoValue> {
    match key {
        "version" | "erts_version" => Ok(InfoValue::Str(ERTS_VERSION.to_string())),
        "otp_release" => Ok(InfoValue::Str(beam_common::otp_version().major.to_string())),
        "system_architecture" => Ok(InfoValue::Str(system_architecture())),
        "system_version" => Ok(InfoValue::Str(format!(
            "Erlang/OTP {} [erts-{ERTS_VERSION}] [source] [{}-bit]",
            beam_common::otp_version().major,
            std::mem::size_of::<usize>() * 8,
        ))),
        "nif_version" => Ok(InfoValue::Str(format!(
            "{ERL_NIF_MAJOR_VERSION}.{ERL_NIF_MINOR_VERSION}"
        ))),
        "wordsize" => Ok(InfoValue::Int(std::mem::size_of::<usize>() as i64)),
        "logical_processors" => Ok(InfoValue::Int(logical_processors())),
        "process_count" => Ok(InfoValue::Int(rt.processes().len() as i64)),
        "os_type" => Ok(os_type()),
        "threads" => Ok(InfoValue::Bool(true)),
        other => Err(error::unknown_info_key(other)),
    }
}

/// The number of logical processors, best-effort.
///
/// `system_info/1` must always answer, so a platform that cannot report the count
/// (rather than an error) falls back to `1` — documented rather than swallowed.
fn logical_processors() -> i64 {
    match std::thread::available_parallelism() {
        Ok(count) => count.get() as i64,
        Err(_) => 1,
    }
}

/// The Erlang `{OsFamily,OsName}` pair of the host.
fn os_type() -> InfoValue {
    if cfg!(target_os = "windows") {
        InfoValue::Tuple("win32", "nt")
    } else {
        InfoValue::Tuple("unix", "linux")
    }
}

/// The target triple of this build, best-effort.
///
/// `std` exposes the architecture and the operating system but not the vendor, so the
/// known targets are spelled out and anything else falls back to
/// `"<arch>-unknown-<os>"`. This is introspection only; the build itself never depends
/// on it.
fn system_architecture() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    match (arch, os) {
        ("x86_64", "linux") => "x86_64-pc-linux-gnu".to_string(),
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu".to_string(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_string(),
        ("aarch64", "macos") => "aarch64-apple-darwin".to_string(),
        _ => format!("{arch}-unknown-{os}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_bifs::BifRegistry;
    use beam_common::OTP_VERSION;
    use beam_process::ProcessId;
    use beam_term::{atom_val, eq, is_atom, is_small, signed_value};

    use crate::boot::seed_core_atoms;
    use crate::config::RuntimeConfig;

    fn runtime() -> Runtime {
        Runtime::new(RuntimeConfig::default()).expect("the default runtime")
    }

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        seed_core_atoms(&mut atoms);
        atoms
    }

    /// The Erlang-string form of `text`, built on `heap` (the oracle's representation).
    fn string_list(heap: &mut Heap, text: &str) -> Eterm {
        let mut list = NIL;
        for byte in text.bytes().rev() {
            list = heap
                .cons(beam_term::make_small(i64::from(byte)), list)
                .expect("the expected list fits");
        }
        list
    }

    #[test]
    fn every_supported_key_answers() {
        let rt = runtime();
        for key in SUPPORTED_KEYS {
            system_info_value(key, &rt)
                .unwrap_or_else(|error| panic!("system_info({key}) failed: {error}"));
        }
        assert_eq!(SUPPORTED_KEYS.len(), 11);
    }

    #[test]
    fn version_keys_and_numbers() {
        let rt = runtime();
        assert_eq!(
            system_info_value("version", &rt).expect("version"),
            InfoValue::Str("17.1".to_string())
        );
        assert_eq!(
            system_info_value("erts_version", &rt).expect("erts_version"),
            InfoValue::Str("17.1".to_string())
        );
        assert_eq!(
            system_info_value("otp_release", &rt).expect("otp_release"),
            InfoValue::Str("30".to_string())
        );
        assert_eq!(
            system_info_value("wordsize", &rt).expect("wordsize"),
            InfoValue::Int(std::mem::size_of::<usize>() as i64)
        );
        assert_eq!(
            system_info_value("nif_version", &rt).expect("nif_version"),
            InfoValue::Str("2.18".to_string())
        );
        assert_eq!(
            system_info_value("threads", &rt).expect("threads"),
            InfoValue::Bool(true)
        );
        assert_eq!(
            system_info_value("os_type", &rt).expect("os_type"),
            InfoValue::Tuple("unix", "linux")
        );
        assert!(matches!(
            system_info_value("process_count", &rt).expect("process_count"),
            InfoValue::Int(count) if count >= 1
        ));
        assert!(matches!(
            system_info_value("logical_processors", &rt).expect("logical_processors"),
            InfoValue::Int(count) if count >= 1
        ));
        assert_eq!(
            system_info_value("system_architecture", &rt).expect("system_architecture"),
            InfoValue::Str(system_architecture())
        );
        assert_eq!(
            system_info_value("system_version", &rt).expect("system_version"),
            InfoValue::Str(format!(
                "Erlang/OTP 30 [erts-17.1] [source] [{}-bit]",
                std::mem::size_of::<usize>() * 8
            ))
        );
    }

    #[test]
    fn unknown_key_is_badarg() {
        let rt = runtime();
        assert!(matches!(
            system_info_value("nonsense", &rt),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(system_info_value("", &rt), Err(BeamError::BadArg)));
        assert!(matches!(
            system_info_value("Version", &rt),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn to_eterm_materializes_every_variant() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();

        let int = InfoValue::Int(8).to_eterm(&mut heap, &atoms).expect("int");
        assert!(is_small(int));
        assert_eq!(signed_value(int), 8);

        let text = InfoValue::Str("17.1".to_string())
            .to_eterm(&mut heap, &atoms)
            .expect("str");
        assert!(eq(text, string_list(&mut heap, "17.1"), &atoms));
        assert!(!eq(text, string_list(&mut heap, "17.2"), &atoms));

        let atom = InfoValue::Atom("undefined")
            .to_eterm(&mut heap, &atoms)
            .expect("atom");
        assert!(is_atom(atom));
        assert_eq!(atoms.name(atom_val(atom)), Some("undefined"));

        let boolean = InfoValue::Bool(false)
            .to_eterm(&mut heap, &atoms)
            .expect("bool");
        assert_eq!(atoms.name(atom_val(boolean)), Some("false"));
        let boolean = InfoValue::Bool(true)
            .to_eterm(&mut heap, &atoms)
            .expect("bool");
        assert_eq!(atoms.name(atom_val(boolean)), Some("true"));

        let tuple = InfoValue::Tuple("unix", "linux")
            .to_eterm(&mut heap, &atoms)
            .expect("tuple");
        let expected = heap
            .tuple(&[
                make_atom(atoms.lookup("unix").expect("seeded")),
                make_atom(atoms.lookup("linux").expect("seeded")),
            ])
            .expect("the expected tuple fits");
        assert!(eq(tuple, expected, &atoms));
    }

    #[test]
    fn to_eterm_reports_a_missing_atom() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = AtomTable::new();
        assert!(matches!(
            InfoValue::Atom("unix").to_eterm(&mut heap, &atoms),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            InfoValue::Tuple("unix", "linux").to_eterm(&mut heap, &atoms),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn empty_string_materializes_to_nil() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        assert_eq!(
            InfoValue::Str(String::new())
                .to_eterm(&mut heap, &atoms)
                .expect("empty"),
            NIL
        );
    }

    #[test]
    fn materialized_values_are_printable_by_the_registered_printer() {
        // The oracle prints `system_info(version)` as the string `"17.1"`, so the
        // runtime's own `~p` printer must render the materialized list the same way.
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let term = InfoValue::Str("17.1".to_string())
            .to_eterm(&mut heap, &atoms)
            .expect("str");
        let registry = BifRegistry::with_core_bifs();
        let rendered = crate::format::term_to_string(
            &registry,
            &mut heap,
            &atoms,
            ProcessId::new(1, 0),
            term,
        )
        .expect("the printer renders it");
        assert_eq!(rendered, "\"17.1\"\n");
    }

    #[test]
    fn otp_version_major_matches_the_constant() {
        assert!(OTP_VERSION.starts_with(&beam_common::otp_version().major.to_string()));
        assert_eq!(beam_common::otp_version().major, 30);
    }
}
