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
//!
//! # Atom-valued keys need a seeded atom table
//!
//! [`InfoValue::to_eterm`] resolves an [`InfoValue::Atom`] **through the runtime's own
//! atom table**; a name that table does not hold is reported as
//! [`BeamError::TermNotFound`], never invented. So the table must hold every atom this
//! surface can emit — [`INFO_ATOMS`] is that complete list. Interning is idempotent and
//! name-keyed, and the boot core seed ([`crate::boot::CORE_ATOMS`]) already interns part
//! of it, so seeding both is safe.

use beam_abi::{ERL_NIF_MAJOR_VERSION, ERL_NIF_MINOR_VERSION};
use beam_alloc::Heap;
use beam_common::ERTS_VERSION;
use beam_common::{BeamError, Result};
use beam_term::{make_atom, try_make_small, AtomTable, Eterm, NIL};

use crate::error;
use crate::runtime::Runtime;

/// The keys [`system_info_value`] answers — the A.8 subset this runtime supports.
pub const SUPPORTED_KEYS: &[&str] = &[
    "version",
    "otp_release",
    "system_architecture",
    "system_version",
    "nif_version",
    "wordsize",
    "logical_processors",
    "process_count",
    "port_count",
    "os_type",
    "threads",
    "emu_flavor",
    "schedulers",
];

/// The **complete** atom vocabulary of this surface — every name [`system_info_value`]
/// can answer as an [`InfoValue::Atom`] (or inside an [`InfoValue::Tuple`]).
///
/// [`InfoValue::to_eterm`] resolves an [`InfoValue::Atom`] through the runtime's own
/// atom table and reports [`BeamError::TermNotFound`] for a name the table does not
/// hold, so the runtime's table must contain these names before any key of
/// [`SUPPORTED_KEYS`] is materialized. It is spelled out here — rather than left to the
/// seeding site — so "the introspection surface is materializable" is checkable in one
/// place (the test below seeds exactly this list and materializes every key). The boot
/// core seed ([`crate::boot::CORE_ATOMS`]) already interns the platform/boolean group;
/// interning is idempotent and name-keyed, so seeding both is harmless.
pub const INFO_ATOMS: &[&str] = &[
    // `os_type` (`{unix,linux}` / `{win32,nt}`) and `threads` (`true` / `false`).
    "unix", "linux", "win32", "nt", "true", "false",
    // `emu_flavor` (`jit` for the JIT emulator, `emu` for the plain interpreter).
    "jit", "emu",
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
    ///   `atoms` — never an invented atom. A runtime materializing this crate's keys
    ///   seeds [`INFO_ATOMS`], which covers every atom the surface can emit;
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
        "version" => Ok(InfoValue::Str(ERTS_VERSION.to_string())),
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
        "port_count" => Ok(InfoValue::Int(port_count(rt)?)),
        "os_type" => Ok(os_type()),
        "threads" => Ok(InfoValue::Bool(true)),
        "emu_flavor" => Ok(emu_flavor(rt)),
        "schedulers" => Ok(InfoValue::Int(schedulers(rt))),
        other => Err(error::unknown_info_key(other)),
    }
}

/// The execution-engine flavor of this runtime, as the oracle reports it.
///
/// Real ERTS answers the atom `jit` for the JIT emulator and `emu` for the plain
/// interpreter. This runtime ships the Stage-1 baseline JIT (`beam_engine`'s
/// `jit::baseline`; workspace `CONTEXT.md` §4) and its emulator configuration defaults
/// to [`Flavor::Jit`](beam_common::config::Flavor::Jit), so
/// `erlang:system_info(emu_flavor)` is the atom `jit` — exactly what the oracle
/// (OTP 29 / erts-17.1) returns.
///
/// The value is **derived from the parsed configuration** rather than hardcoded, so an
/// `-emu_flavor emu` invocation answers `emu` as soon as the CLI maps that switch onto
/// the emulator configuration (Milestone-1 records `-emu_flavor` as an init argument
/// only, so the flavor stays at its `jit` default).
///
/// The answer is an atom, so the runtime's atom table must hold it — see [`INFO_ATOMS`].
fn emu_flavor(rt: &Runtime) -> InfoValue {
    InfoValue::Atom(rt.config().emulator.flavor.as_str())
}

/// The number of **normal schedulers**, as the oracle reports it.
///
/// An explicit `+S N` wins. `+S 0` means "auto" in real ERTS (`erl +S 0` reports the
/// online processor count), so it falls through to the machine's parallelism as well —
/// the same answer the oracle gives without `+S`. The count is always `> 0`: the
/// scheduler *loop* is Stage-5 work (`beam_process`), but a count of `0` schedulers
/// would be a lie about a runtime that executes code.
fn schedulers(rt: &Runtime) -> i64 {
    match rt.config().emulator.schedulers {
        Some(count) if count > 0 => i64::from(count),
        _ => logical_processors(),
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

/// The number of **live ports** — the value the oracle's `port_count` reports.
///
/// The runtime's own [`PortRegistry`](crate::port::PortRegistry) is the single source of
/// truth: a port counts once it is *live* (`RuntimePorts` inserts it), which is exactly
/// when the oracle's own `port_count` rises. A fresh runtime has opened no port, so the
/// honest answer is `0` (the oracle boots with its `standard_io` port already open, which
/// this runtime has no counterpart for).
///
/// That divergence is **deliberate and must not be "fixed"** to match a run-time figure:
/// like `erlang:ports/0`, this key reports *this* runtime's table, not the oracle's (see
/// the same note in [`crate::port`]).
///
/// The registry is shared through an [`Rc`]`<`[`std::cell::RefCell`]`>` so a port BIF in
/// flight can hold it without borrowing the runtime; a `RefCell` conflict is reported as
/// the typed capability gap rather than panicking — the crate is `#![deny(unsafe_code)]`
/// and the same rule `erlang:ports/0` follows.
fn port_count(rt: &Runtime) -> Result<i64> {
    match rt.ports.try_borrow() {
        Ok(registry) => Ok(registry.len() as i64),
        Err(_) => Err(error::unsupported(
            "erlang:system_info(port_count): the port registry is already borrowed",
        )),
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
        assert_eq!(SUPPORTED_KEYS.len(), 13);
    }

    #[test]
    fn every_supported_key_materializes_once_the_info_atoms_are_seeded() {
        // `INFO_ATOMS` is the whole contract the introspection surface has with the atom
        // table: seeding exactly this list must materialize **every** supported key (the
        // oracle's `system_info/1` never returns an unrepresentable value).
        let rt = runtime();
        let mut heap = Heap::new(1_024).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(INFO_ATOMS.iter().copied());
        for key in SUPPORTED_KEYS {
            let value = system_info_value(key, &rt)
                .unwrap_or_else(|error| panic!("system_info({key}) failed: {error}"));
            value
                .to_eterm(&mut heap, &atoms)
                .unwrap_or_else(|error| panic!("system_info({key}) does not materialize: {error}"));
        }
    }

    #[test]
    fn the_non_standard_erts_version_alias_is_gone() {
        // Real ERTS raises `badarg` for `erts_version`; so does this runtime now.
        let rt = runtime();
        assert!(matches!(
            system_info_value("erts_version", &rt),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn version_keys_and_numbers() {
        let rt = runtime();
        assert_eq!(
            system_info_value("version", &rt).expect("version"),
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
        ));        assert_eq!(
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
    fn port_count_is_the_live_port_count() {
        use beam_engine::{terms, ExecutionContext};

        // A fresh runtime has opened no port (the oracle's own boot opens a
        // `standard_io` port that this runtime has no counterpart for).
        let mut rt = runtime();
        assert_eq!(
            system_info_value("port_count", &rt).expect("port_count"),
            InfoValue::Int(0)
        );

        // Open a real `{spawn, "true"}` port through `erlang:open_port/2`: the count rises
        // while it is live and drops once it is closed, so the answer tracks the live port
        // set exactly. (The childless `{fd, N}` form needs a descriptor the caller owns —
        // adopting an *inherited* one is refused here by `fcntl` with `EPERM`; it is
        // covered instead by `port::tests::a_fd_port_adopts_a_caller_owned_descriptor`.)
        let port = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut rt, &mut sink);
            let spawn = context.atom("spawn").expect("the `spawn` atom");
            let command = {
                let bytes = beam_engine::terms::make_list(
                    &mut context,
                    &"true"
                        .bytes()
                        .map(|b| beam_term::make_small(i64::from(b)))
                        .collect::<Vec<_>>(),
                )
                .expect("the command string");
                bytes
            };
            let name = terms::make_tuple(&mut context, &[spawn, command]).expect("a {spawn, Cmd}");
            let options = terms::make_list(&mut context, &[]).expect("[]");
            context
                .call_bif("erlang", "open_port", 2, &[name, options])
                .expect("erlang:open_port/2")
        };
        assert_eq!(
            system_info_value("port_count", &rt).expect("port_count"),
            InfoValue::Int(1),
            "the opened port is counted"
        );
        // `erlang:ports/0` reports exactly the same one port.
        let ports = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut rt, &mut sink);
            context
                .call_bif("erlang", "ports", 0, &[])
                .expect("erlang:ports/0")
        };
        assert_eq!(terms::list_elements(ports).expect("a list"), vec![port]);

        {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut rt, &mut sink);
            context
                .call_bif("erlang", "port_close", 1, &[port])
                .expect("erlang:port_close/1");
        }
        assert_eq!(
            system_info_value("port_count", &rt).expect("port_count"),
            InfoValue::Int(0),
            "a closed port is no longer counted"
        );
    }

    #[test]
    fn emu_flavor_is_the_configured_flavor_atom() {
        // The oracle (OTP 29 / erts-17.1) answers `jit` for its JIT emulator; the Rust
        // runtime ships the Stage-1 baseline JIT and defaults to `Flavor::Jit`, so it
        // answers the same atom.
        let rt = runtime();
        assert_eq!(
            system_info_value("emu_flavor", &rt).expect("emu_flavor"),
            InfoValue::Atom("jit")
        );

        // The value follows the emulator configuration instead of being hardcoded, so a
        // runtime configured with `-emu_flavor emu` reports `emu` (the oracle's other
        // legal answer) rather than a stale `jit`.
        let mut config = RuntimeConfig::default();
        config.emulator = config.emulator.with_flavor(beam_common::Flavor::Emu);
        let emu = Runtime::new(config).expect("a runtime for the emu flavor");
        assert_eq!(
            system_info_value("emu_flavor", &emu).expect("emu_flavor"),
            InfoValue::Atom("emu")
        );
    }

    #[test]
    fn emu_flavor_materializes_to_the_flavor_atom() {
        let mut heap = Heap::new(64).expect("test heap");
        let mut atoms = seeded_atoms();
        let jit = atoms.intern("jit");
        let term = InfoValue::Atom("jit")
            .to_eterm(&mut heap, &atoms)
            .expect("the jit atom");
        assert!(is_atom(term));
        assert_eq!(atom_val(term), jit);
        assert_eq!(atoms.name(atom_val(term)), Some("jit"));

        // An atom the table does not hold cannot be resolved: an absent atom is reported,
        // not silently replaced by some other atom. `seed_core_atoms` now interns the
        // whole `INFO_ATOMS` vocabulary (see `crate::boot`), so a genuinely empty table is
        // what exercises the absent-atom path.
        let mut unseeded = AtomTable::new();
        assert!(matches!(
            InfoValue::Atom("jit").to_eterm(&mut heap, &unseeded),
            Err(BeamError::TermNotFound)
        ));
        unseeded.seed(INFO_ATOMS.iter().copied());
        assert!(is_atom(
            InfoValue::Atom("jit")
                .to_eterm(&mut heap, &unseeded)
                .expect("seeded from INFO_ATOMS")
        ));
    }

    #[test]
    fn schedulers_is_the_configured_or_online_count() {
        // The oracle reports an integer > 0 (its online schedulers = logical processors).
        let rt = runtime();
        let count = match system_info_value("schedulers", &rt).expect("schedulers") {
            InfoValue::Int(count) => count,
            other => panic!("schedulers must be an integer, got {other:?}"),
        };
        assert!(
            count > 0,
            "a runtime has at least one scheduler, got {count}"
        );

        // An explicit `+S N` wins (`erl +S 3` reports 3).
        let configured =
            Runtime::new(RuntimeConfig::default().with_schedulers(3)).expect("a runtime with +S 3");
        assert_eq!(
            system_info_value("schedulers", &configured).expect("schedulers"),
            InfoValue::Int(3)
        );

        // `+S 0` means "auto" in real ERTS, so it must not be reported as 0.
        let auto =
            Runtime::new(RuntimeConfig::default().with_schedulers(0)).expect("a runtime with +S 0");
        assert!(matches!(
            system_info_value("schedulers", &auto).expect("schedulers"),
            InfoValue::Int(count) if count > 0
        ));
    }

    #[test]
    fn schedulers_materializes_to_a_small_integer() {        let rt = runtime();
        let value = system_info_value("schedulers", &rt).expect("schedulers");
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded_atoms();
        let term = value.to_eterm(&mut heap, &atoms).expect("materialize");
        assert!(is_small(term));
        assert!(signed_value(term) > 0);
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
