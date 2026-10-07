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
///
/// The list holds **atom** keys only; the one tuple key the oracle accepts that this
/// runtime answers, `{allocator, A}`, is handled by the tuple path in
/// [`crate::native`]'s `system_info/1` (it is dispatched to [`allocator_report`]).
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
    "alloc_util_allocators",
    "allocated_areas",
    "garbage_collection",
];

/// The **complete** atom vocabulary of this surface — every name [`system_info_value`]
/// can answer as an [`InfoValue::Atom`] (or nested inside one of its list/tuple
/// variants), and every name [`crate::native`]'s `process_info/2` can emit.
///
/// [`InfoValue::to_eterm`] resolves an [`InfoValue::Atom`] through the runtime's own
/// atom table and reports [`BeamError::TermNotFound`] for a name the table does not
/// hold, so the runtime's table must contain these names before any key of
/// [`SUPPORTED_KEYS`] is materialized. It is spelled out here — rather than left to the
/// seeding site — so "the introspection surface is materializable" is checkable in one
/// place (the test below seeds exactly this list and materializes every key). The boot
/// core seed ([`crate::boot::seed_core_atoms`]) interns this whole list after
/// [`crate::boot::CORE_ATOMS`]; interning is idempotent and name-keyed, so overlapping
/// with the core set is harmless, and **appending** to this list keeps every index the
/// core seed assigned before it unchanged.
pub const INFO_ATOMS: &[&str] = &[
    // `os_type` (`{unix,linux}` / `{win32,nt}`) and `threads` (`true` / `false`).
    "unix", "linux", "win32", "nt", "true", "false",
    // `emu_flavor` (`jit` for the JIT emulator, `emu` for the plain interpreter).
    "jit", "emu",
    // `alloc_util_allocators`: the ten alloc-util allocator names, in the oracle's order
    // (verified against OTP 29/erts-17.1). `beam_alloc::registry::alloc_util_allocators`
    // yields exactly these, in exactly this order.
    "temp_alloc",
    "sl_alloc",
    "std_alloc",
    "ll_alloc",
    "eheap_alloc",
    "ets_alloc",
    "fix_alloc",
    "literal_alloc",
    "binary_alloc",
    "driver_alloc",
    // `allocated_areas`: the oracle's 19 item names, in order.
    "sys_misc",
    "static",
    "atom_space",
    "atom_table",
    "module_table",
    "export_table",
    "export_list",
    "register_table",
    "fun_table",
    "fun_list",
    "module_refs",
    "loaded_code",
    "dist_table",
    "node_table",
    "bif_timer",
    "process_table",
    "port_table",
    "ets_misc",
    "external_alloc",
    // `{allocator, A}`: the tuple tag plus the row names
    // `beam_alloc::stats::AllocatorInfo::from_stats` emits (`info_carriers`/`info_calls`
    // in `erl_alloc_util.c`, `info_status` in `erl_mseg.c`).
    "allocator",
    "blocks",
    "bytes",
    "carriers",
    "carriers size",
    "sbcs carriers",
    "mbcs carriers",
    "alloc calls",
    "free calls",
    "realloc calls",
    "mseg carriers",
    "mseg carriers size",
    "mseg cache_hits",
    "mseg unmaps",
    // `garbage_collection` (the `system_info/1` key and the `process_info/2` item): the
    // five item names in the oracle's emitted order (`erl_bif_info.c:2082-2093`).
    "garbage_collection",
    "max_heap_size",
    "min_bin_vheap_size",
    "min_heap_size",
    "fullsweep_after",
    "minor_gcs",
    // The `max_heap_size` process-flag map (`process_info(Pid, garbage_collection)`).
    "error_logger",
    "include_shared_binaries",
    "kill",
    "size",
    // The remaining `process_info/2` items and the reply for a pid that is not live.
    "heap_size",
    "total_heap_size",
    "undefined",
];

/// A `system_info/1` value, independent of any heap.
///
/// The first five variants are the scalar shapes; [`InfoValue::List`] and
/// [`InfoValue::Tupled`] are the generic containers the richer keys
/// (`alloc_util_allocators`, `allocated_areas`, `{allocator, A}`,
/// `garbage_collection`) are built from — a `{Key, Value}` row is
/// `Tupled(vec![Atom(key), value])`, so the oracle's proplists and its mixed-arity
/// tuples (`{atom_space, Bytes, Bytes}`) are both expressible.
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
    /// A proper list, in order (materialized element by element, head first).
    List(Vec<InfoValue>),
    /// A tuple of arbitrary arity, in order.
    Tupled(Vec<InfoValue>),
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
    /// * the heap's allocation failure for a string, a list or a tuple.
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
            InfoValue::List(elements) => {
                // Materialize the elements first (they may need heap space of their own),
                // then build the spine, so a nested container cannot be half-written.
                let mut terms = Vec::with_capacity(elements.len());
                for element in elements {
                    terms.push(element.to_eterm(heap, atoms)?);
                }
                let mut list = NIL;
                for term in terms.into_iter().rev() {
                    list = heap.cons(term, list)?;
                }
                Ok(list)
            }
            InfoValue::Tupled(elements) => {
                let mut terms = Vec::with_capacity(elements.len());
                for element in elements {
                    terms.push(element.to_eterm(heap, atoms)?);
                }
                heap.tuple(&terms)
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
        "alloc_util_allocators" => Ok(alloc_util_allocators()),
        "allocated_areas" => allocated_areas(rt),
        "garbage_collection" => Ok(garbage_collection()),
        other => Err(error::unknown_info_key(other)),
    }
}

// -- the allocator/GC introspection keys (`beam_alloc` / `beam_gc` adoption) -----------

/// `system_info(alloc_util_allocators)` — the *alloc-util* allocator names.
///
/// Verified against the oracle (OTP 29 / erts-17.1):
/// `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(alloc_util_allocators)]).'`
/// prints `[temp_alloc,sl_alloc,std_alloc,ll_alloc,eheap_alloc,ets_alloc,fix_alloc,
/// literal_alloc,binary_alloc,driver_alloc]` — which is exactly
/// [`beam_alloc::registry::alloc_util_allocators`] (the C
/// `erts_alloc_util_allocators`, `erl_alloc.c:2789`, i.e. every [`AllocatorId`] but
/// `sys_alloc` — the raw system allocator — and `test_alloc`), in the same order.
///
/// [`AllocatorId`]: beam_alloc::types::AllocatorId
fn alloc_util_allocators() -> InfoValue {
    InfoValue::List(
        beam_alloc::registry::alloc_util_allocators()
            .into_iter()
            .map(InfoValue::Atom)
            .collect(),
    )
}

/// The **effective** default heap policy: the emulator defaults a process started
/// without `spawn_opt` heap options gets.
///
/// [`beam_gc::HeapPolicy::default`] is the *unbounded floor*
/// (`min_heap_words == 0`), whereas the oracle reports the C's `H_DEFAULT_SIZE`
/// (233 words). [`beam_gc::HeapPolicy::from_spawn_opts`] applies exactly that floor (and
/// the C's binary-vheap floor) to a requested value, so routing the defaults through it
/// yields the oracle's `min_heap_size` (233), `min_bin_vheap_size` (46422) and
/// `fullsweep_after` (65535) **without hardcoding any of them** — the numbers come from
/// `beam_gc`'s growth table, which is itself derived from `erl_init.c`.
fn effective_default_policy() -> beam_gc::HeapPolicy {
    effective_policy(&beam_gc::HeapPolicy::default())
}

/// The policy a collector started with `base` reports to the oracle.
///
/// `beam_gc`'s [`HeapPolicy::default`](beam_gc::HeapPolicy::default) keeps
/// `min_heap_words` at 0 (an *unbounded floor*), but every process the runtime spawns
/// goes through the C's spawn path, whose minimum is `H_DEFAULT_SIZE` (233 words). This
/// is the same conversion [`beam_gc::HeapPolicy::from_spawn_opts`] applies, so a process
/// with no explicit heap options reports the emulator default rather than the sentinel.
pub(crate) fn effective_policy(base: &beam_gc::HeapPolicy) -> beam_gc::HeapPolicy {
    beam_gc::HeapPolicy::from_spawn_opts(
        base.min_heap_words,
        base.min_bin_vheap_words,
        base.fullsweep_after,
    )
}

/// `system_info(garbage_collection)` — the emulator-wide GC policy knobs.
///
/// Verified against the oracle (OTP 29 / erts-17.1): `[{max_heap_size,0},
/// {min_bin_vheap_size,46422},{min_heap_size,233},{fullsweep_after,65535}]`. Note that
/// `max_heap_size` is the **bare integer** here, unlike the flag *map*
/// `process_info(Pid, garbage_collection)` reports.
///
/// The values are the [`effective_default_policy`] bounds, i.e. the same figures the
/// per-process item reports for a process with no explicit heap options — the four keys
/// [`beam_gc::GARBAGE_COLLECTION_KEYS`] names minus `minor_gcs`, which is a per-process
/// counter with no emulator-wide counterpart.
fn garbage_collection() -> InfoValue {
    // `HeapInfo` owns the oracle's item order; the sizes are irrelevant for this key.
    let info = beam_gc::HeapInfo::new(&effective_default_policy(), 0, 0, 0);
    let items = info.items();
    InfoValue::List(
        items[..4]
            .iter()
            .map(|(key, value)| row(key, usize_to_int(*value)))
            .collect(),
    )
}

/// `system_info(allocated_areas)` — the runtime's own allocation-area footprints, in the
/// oracle's 19-item order and arity.
///
/// Verified against the oracle (OTP 29 / erts-17.1), which emits exactly these names in
/// this order (the C `erts_allocated_areas`, `erl_alloc.c:2580-2699`):
/// `sys_misc`, `static`, `atom_space` (a **three**-element tuple), `atom_table`,
/// `module_table`, `export_table`, `export_list`, `register_table`, `fun_table`,
/// `fun_list`, `module_refs`, `loaded_code`, `dist_table`, `node_table`, `bif_timer`,
/// `process_table`, `port_table`, `ets_misc`, `external_alloc`.
///
/// The oracle's *values* are run-dependent byte counts of a completely different
/// emulator, so none of them can be mirrored. Every value here is instead **derived from
/// state this runtime actually has** — never a hardcoded oracle figure, which would be a
/// lie about a different process:
///
/// | item | value |
/// |---|---|
/// | `atom_space` | `{atom_space, B, B}`, `B` = the atom table's index slots |
/// | `atom_table` | `B` |
/// | `module_table`, `loaded_code` | one slot per **loaded** code module |
/// | `register_table` | one slot per locally **registered** name |
/// | `process_table` | one slot per **live** process |
/// | `port_table` | one slot per **live** port |
/// | `ets_misc` | one slot per **live** ETS table |
/// | every other item | `0` |
///
/// A "slot" is [`std::mem::size_of::<usize>`] bytes — the word the runtime's own tables
/// spend per entry — so the figures are a real (if coarse) lower bound on this runtime's
/// footprint, in the byte unit the oracle uses. The `0` items are the ones this runtime
/// has **no counterpart** for (no `export`/BIF-timer/`fun` table, no distribution or node
/// table, no external port-driver allocator), and a `0` is honest where an invented
/// figure would not be.
fn allocated_areas(rt: &Runtime) -> Result<InfoValue> {
    let slot = std::mem::size_of::<usize>() as i64;
    let atoms = slot * rt.code_server.atoms().len() as i64;
    let modules = slot * rt.loaded_modules().len() as i64;
    let registered = slot * rt.processes().registered().count() as i64;
    let processes = slot * rt.processes().len() as i64;
    let ports = slot * live_ports(rt)?;
    let tables = slot * live_tables(rt)?;
    Ok(InfoValue::List(vec![
        row("sys_misc", 0),
        row("static", 0),
        InfoValue::Tupled(vec![
            InfoValue::Atom("atom_space"),
            InfoValue::Int(atoms),
            InfoValue::Int(atoms),
        ]),
        row("atom_table", atoms),
        row("module_table", modules),
        row("export_table", 0),
        row("export_list", 0),
        row("register_table", registered),
        row("fun_table", 0),
        row("fun_list", 0),
        row("module_refs", 0),
        row("loaded_code", modules),
        row("dist_table", 0),
        row("node_table", 0),
        row("bif_timer", 0),
        row("process_table", processes),
        row("port_table", ports),
        row("ets_misc", tables),
        row("external_alloc", 0),
    ]))
}

/// A `{Key, Value}` row of an `InfoValue` proplist.
fn row(key: &'static str, value: i64) -> InfoValue {
    InfoValue::Tupled(vec![InfoValue::Atom(key), InfoValue::Int(value)])
}

/// The number of live ports, or the typed capability gap when the shared port registry is
/// already borrowed (the same rule [`port_count`] follows — never a panic).
fn live_ports(rt: &Runtime) -> Result<i64> {
    match rt.ports.try_borrow() {
        Ok(registry) => Ok(registry.len() as i64),
        Err(_) => Err(error::unsupported(
            "erlang:system_info(allocated_areas): the port registry is already borrowed",
        )),
    }
}

/// The number of live ETS tables, or the typed capability gap when the shared table
/// registry is already borrowed.
fn live_tables(rt: &Runtime) -> Result<i64> {
    match rt.tables.try_borrow() {
        Ok(tables) => Ok(tables.len() as i64),
        Err(_) => Err(error::unsupported(
            "erlang:system_info(allocated_areas): the ETS table registry is already borrowed",
        )),
    }
}

/// `system_info({allocator, A})` — the measured counters of the alloc-util allocator `A`,
/// as a list of `{Key, Value}` rows.
///
/// This is the `beam_alloc` handoff the crate `CONTEXT.md` names: the rows are
/// [`beam_alloc::stats::AllocatorInfo::from_stats`] over a **live** snapshot
/// ([`beam_alloc::registry::all_stats`]), so every figure is a real measurement of *this*
/// process — the allocator name, the carrier counts, the call counters and the
/// process-global segment statistics of `beam_alloc`'s registry.
///
/// # Documented divergence from the oracle
///
/// The oracle wraps the same underlying counters in one `{instance, N, [...]}` entry per
/// allocator **instance** (`N` = the scheduler count; on this machine
/// `erl -noshell -eval 'io:format("~p~n",[erlang:system_info({allocator,std_alloc})]).'`
/// prints 17 `{instance, …}` entries, each with `versions`/`options`/`mbcs`/`sbcs`/
/// `calls` sub-items). This runtime has a **single** allocator per type
/// (`beam_alloc::registry`) and models no per-instance carrier pool or `versions` string,
/// so it reports the flat, real `{Key, Value}` row set `beam_alloc` can actually produce
/// rather than inventing an instance count or a version string.
///
/// # Errors
///
/// [`BeamError::BadArg`] for an atom that is not one of the ten alloc-util allocator
/// names — the oracle's answer for an unknown name (`system_info({allocator, foo_alloc})`
/// ⇒ `error:badarg`).
pub fn allocator_report(name: &str) -> Result<InfoValue> {
    let static_name = beam_alloc::registry::alloc_util_allocators()
        .into_iter()
        .find(|candidate| *candidate == name)
        .ok_or(BeamError::BadArg)?;
    let stats = beam_alloc::registry::all_stats()
        .into_iter()
        .find(|(allocator, _)| allocator.as_str() == static_name)
        .map(|(_, stats)| stats)
        .ok_or(BeamError::BadArg)?;
    let report = beam_alloc::stats::AllocatorInfo::from_stats(static_name, &stats);
    let mut rows = Vec::with_capacity(report.items.len());
    for item in &report.items {
        rows.push(InfoValue::Tupled(vec![
            InfoValue::Atom(item.key),
            allocator_value(item.value)?,
        ]));
    }
    Ok(InfoValue::List(rows))
}

/// One [`beam_alloc`](beam_alloc::stats::InfoValue) report value as an [`InfoValue`].
///
/// A counter wider than a 60-bit small integer would not be materializable, so it is the
/// typed capability gap ([`BeamError::SystemLimit`]) rather than a truncated number.
fn allocator_value(value: beam_alloc::stats::InfoValue) -> Result<InfoValue> {
    match value {
        beam_alloc::stats::InfoValue::U64(value) => usize_to_int_result(value as usize),
        beam_alloc::stats::InfoValue::Usize(value) => usize_to_int_result(value),
        beam_alloc::stats::InfoValue::Text(text) => Ok(InfoValue::Str(text.to_string())),
    }
}

/// `value` as an [`InfoValue::Int`], or [`BeamError::SystemLimit`] when it cannot be a
/// small integer.
fn usize_to_int_result(value: usize) -> Result<InfoValue> {
    i64::try_from(value)
        .map(InfoValue::Int)
        .map_err(|_| BeamError::SystemLimit)
}

/// `value` as an `i64`, saturating at the small-integer range's practical bound.
///
/// Used only for the table-footprint figures of [`allocated_areas`], where the operand is
/// a table length times the word size — far inside the range on any real machine.
fn usize_to_int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
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
        assert_eq!(SUPPORTED_KEYS.len(), 16);
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
    fn alloc_util_allocators_matches_the_oracle_list() {
        // Pinned against the oracle (OTP 29 / erts-17.1):
        // `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(alloc_util_allocators)]).'`
        // — set **and** order.
        let rt = runtime();
        let value = system_info_value("alloc_util_allocators", &rt).expect("alloc_util_allocators");
        let names: Vec<&str> = match &value {
            InfoValue::List(elements) => elements
                .iter()
                .map(|element| match element {
                    InfoValue::Atom(name) => *name,
                    other => panic!("an allocator name must be an atom, got {other:?}"),
                })
                .collect(),
            other => panic!("alloc_util_allocators must be a list, got {other:?}"),
        };
        assert_eq!(
            names,
            [
                "temp_alloc",
                "sl_alloc",
                "std_alloc",
                "ll_alloc",
                "eheap_alloc",
                "ets_alloc",
                "fix_alloc",
                "literal_alloc",
                "binary_alloc",
                "driver_alloc",
            ]
        );
    }

    #[test]
    fn allocators_is_badarg_like_the_oracle() {
        // Verified: `erlang:system_info(allocators)` raises `error:badarg` in OTP 29 /
        // erts-17.1, so this runtime must not answer it either.
        let rt = runtime();
        assert!(matches!(
            system_info_value("allocators", &rt),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn allocated_areas_has_the_oracle_names_arities_and_derivations() {
        // The oracle's 19 items, in order (`erts_allocated_areas`,
        // `erl_alloc.c:2580-2699`), with `atom_space` as its three-element tuple.
        let rt = runtime();
        let value = system_info_value("allocated_areas", &rt).expect("allocated_areas");
        let rows = match &value {
            InfoValue::List(rows) => rows,
            other => panic!("allocated_areas must be a list, got {other:?}"),
        };
        let expected = [
            "sys_misc",
            "static",
            "atom_space",
            "atom_table",
            "module_table",
            "export_table",
            "export_list",
            "register_table",
            "fun_table",
            "fun_list",
            "module_refs",
            "loaded_code",
            "dist_table",
            "node_table",
            "bif_timer",
            "process_table",
            "port_table",
            "ets_misc",
            "external_alloc",
        ];
        let pairs: Vec<(String, Vec<i64>)> = rows
            .iter()
            .map(|row| match row {
                InfoValue::Tupled(fields) => {
                    let name = match &fields[0] {
                        InfoValue::Atom(name) => (*name).to_string(),
                        other => panic!("a row's first field must be an atom, got {other:?}"),
                    };
                    let values = fields[1..]
                        .iter()
                        .map(|field| match field {
                            InfoValue::Int(value) => *value,
                            other => panic!("{name}'s value must be an integer, got {other:?}"),
                        })
                        .collect();
                    (name, values)
                }
                other => panic!("a row must be a tuple, got {other:?}"),
            })
            .collect();
        let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, expected);
        assert_eq!(pairs[2].1.len(), 2, "atom_space carries a value and a peak");

        // The measured items reflect *this* runtime's own state (never a hardcoded oracle
        // figure): the atom table always holds the boot seed and the runtime always has
        // its root process, while the items with no counterpart are honestly `0`.
        let value_of = |name: &str| -> i64 {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .unwrap_or_else(|| panic!("{name} must be present"))
                .1[0]
        };
        assert!(value_of("atom_table") > 0, "the atom table holds the core seed");
        assert!(value_of("process_table") > 0, "the runtime has its root process");
        for absent in [
            "bif_timer",
            "dist_table",
            "node_table",
            "external_alloc",
            "export_table",
            "fun_table",
        ] {
            assert_eq!(value_of(absent), 0, "{absent} has no counterpart here");
        }
    }

    #[test]
    fn garbage_collection_matches_the_oracle_list() {
        // Pinned against the oracle (OTP 29 / erts-17.1):
        // `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(garbage_collection)]).'`
        // prints `[{max_heap_size,0},{min_bin_vheap_size,46422},{min_heap_size,233},
        // {fullsweep_after,65535}]` — `max_heap_size` is the **bare integer** here, and
        // none of the values is hardcoded: they come from `beam_gc`'s growth table.
        let rt = runtime();
        let value = system_info_value("garbage_collection", &rt).expect("garbage_collection");
        let expected = [
            ("max_heap_size", 0_i64),
            ("min_bin_vheap_size", 46_422),
            ("min_heap_size", 233),
            ("fullsweep_after", 65_535),
        ];
        let rows = match &value {
            InfoValue::List(rows) => rows,
            other => panic!("garbage_collection must be a list, got {other:?}"),
        };
        assert_eq!(rows.len(), expected.len());
        for (row, (key, expected_value)) in rows.iter().zip(expected) {
            assert_eq!(
                *row,
                InfoValue::Tupled(vec![InfoValue::Atom(key), InfoValue::Int(expected_value)])
            );
        }
    }

    #[test]
    fn allocator_report_carries_the_measured_counters() {
        let value = allocator_report("std_alloc").expect("std_alloc is an alloc-util allocator");
        let rows = match &value {
            InfoValue::List(rows) => rows,
            other => panic!("an allocator report must be a list, got {other:?}"),
        };
        let keys: Vec<&str> = rows
            .iter()
            .map(|row| match row {
                InfoValue::Tupled(fields) => match &fields[0] {
                    InfoValue::Atom(key) => *key,
                    other => panic!("a row key must be an atom, got {other:?}"),
                },
                other => panic!("a row must be a tuple, got {other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            [
                "blocks",
                "bytes",
                "carriers",
                "carriers size",
                "sbcs carriers",
                "mbcs carriers",
                "alloc calls",
                "free calls",
                "realloc calls",
                "mseg carriers",
                "mseg carriers size",
                "mseg cache_hits",
                "mseg unmaps",
            ]
        );
        // The report is a live measurement: every value exists and is non-negative.
        for row in rows {
            let InfoValue::Tupled(fields) = row else {
                panic!("a row must be a tuple, got {row:?}");
            };
            assert!(
                matches!(&fields[1], InfoValue::Int(value) if *value >= 0),
                "{row:?}"
            );
        }
    }

    #[test]
    fn allocator_report_rejects_a_name_that_is_not_an_alloc_util_allocator() {
        // The oracle raises `error:badarg` for an unknown allocator name
        // (`system_info({allocator, foo_alloc})`).
        for name in ["foo_alloc", "std", "sys_alloc", "test_alloc", "", "eheap"] {
            assert!(
                matches!(allocator_report(name), Err(BeamError::BadArg)),
                "{name} must be badarg"
            );
        }
        // Every one of the ten alloc-util names answers with its own report.
        let names = beam_alloc::registry::alloc_util_allocators();
        assert_eq!(names.len(), 10);
        for name in names {
            assert!(allocator_report(name).is_ok(), "{name} must answer");
        }
    }

    #[test]
    fn the_container_variants_materialize() {
        use beam_engine::terms;

        let mut heap = Heap::new(1_024).expect("test heap");
        let atoms = seeded_atoms();
        let term = InfoValue::List(vec![
            InfoValue::Atom("unix"),
            InfoValue::Tupled(vec![InfoValue::Atom("linux"), InfoValue::Int(3)]),
        ])
        .to_eterm(&mut heap, &atoms)
        .expect("materialize");
        let elements = terms::list_elements(term).expect("a list");
        assert_eq!(elements.len(), 2);
        assert_eq!(atoms.name(atom_val(elements[0])), Some("unix"));
        assert_eq!(terms::tuple_arity(elements[1]).expect("a tuple"), 2);
        assert_eq!(
            signed_value(terms::tuple_element(elements[1], 1).expect("an element")),
            3
        );

        // An empty list is `[]` and a nullary tuple is legal too.
        assert_eq!(
            InfoValue::List(Vec::new())
                .to_eterm(&mut heap, &atoms)
                .expect("nil"),
            NIL
        );
        assert_eq!(
            terms::tuple_arity(
                InfoValue::Tupled(Vec::new())
                    .to_eterm(&mut heap, &atoms)
                    .expect("{}")
            )
            .expect("a tuple"),
            0
        );

        // A missing atom *inside* a container is reported, never invented.
        let mut unseeded = AtomTable::new();
        assert!(matches!(
            InfoValue::List(vec![InfoValue::Atom("unix")]).to_eterm(&mut heap, &unseeded),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            InfoValue::Tupled(vec![InfoValue::Int(1), InfoValue::Atom("linux")])
                .to_eterm(&mut heap, &unseeded),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn otp_version_major_matches_the_constant() {
        assert!(OTP_VERSION.starts_with(&beam_common::otp_version().major.to_string()));
        assert_eq!(beam_common::otp_version().major, 30);
    }
}
