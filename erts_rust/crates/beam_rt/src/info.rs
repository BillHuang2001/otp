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
//! # Key families (the module split)
//!
//! The key surface is large, so this parent keeps only the materialization core —
//! [`InfoValue`], [`InfoValue::to_eterm`], the shared helpers, [`INFO_ATOMS`] and the
//! [`system_info_value`] dispatcher — and delegates the per-key logic to child
//! modules, each exposing `pub(crate) fn try_value(key, rt) -> Option<Result<InfoValue>>`
//! (`None` means "not my key"):
//!
//! | module | keys |
//! |---|---|
//! | [`host`] | the host-derived `os_type` / `system_architecture` pair (used by [`system`]) |
//! | [`system`] | build/host identity constants (`version`, `nif_version`, `os_type`, `emu_type`, …) |
//! | [`limits`] | counts read from this runtime's live tables plus the emulator limits (`atom_count`, `process_count`, `port_limit`, …) |
//! | [`schedulers`] | processor/scheduler/multi-scheduling keys |
//! | [`memory`] | GC/heap policy keys, `allocated_areas`, `alloc_util_allocators`, `garbage_collection` |
//! | [`dist`] | the distribution-state keys `dist` / `dist_ctrl` (answered only in the oracle's non-distributed state) |
//! | [`time`] | the ERTS monotonic-time interface constants `start_time` / `end_time` |
//! | [`unsupported`] | the oracle's keys for facilities this runtime does not have — an honest capability gap |
//!
//! # Implemented vs unsupported vs badarg
//!
//! Three disjoint key sets (documented per key in the family modules and in `CONTEXT.md`):
//!
//! * [`SUPPORTED_KEYS`] — answered with the oracle's exact value *shape*. Some are
//!   genuine constants of this build/host; others are **self-derived** from this
//!   runtime's own live tables (`process_count`, `atom_count`, `allocated_areas`, …) and
//!   are therefore correct-but-smaller than the oracle's run-time figure.
//! * [`UNSUPPORTED_KEYS`] — the oracle accepts the key but it describes a facility this
//!   runtime genuinely does not have (the time-correction service, CPU-topology detection,
//!   the C-compiler/3PP identification, the code-loading dumps, the poll-set
//!   introspection, …). Each
//!   returns a raise whose reason atom starts with `unsupported`
//!   ([`crate::error::unsupported`]) so a `-eval` caller sees the exit-3 capability gap —
//!   **never** a fabricated value or a hardcoded oracle number.
//! * every other key — `badarg`, exactly what the oracle raises for a key it does not
//!   accept (`erlang:system_info(nonsense)` ⇒ `error:badarg`).
//!
//! # Atom-valued keys need a seeded atom table
//!
//! [`InfoValue::to_eterm`] resolves an [`InfoValue::Atom`] **through the runtime's own
//! atom table**; a name that table does not hold is reported as
//! [`BeamError::TermNotFound`], never invented. So the table must hold every atom this
//! surface can emit — [`INFO_ATOMS`] is that complete list. Interning is idempotent and
//! name-keyed, and the boot core seed ([`crate::boot::CORE_ATOMS`]) already interns part
//! of it, so seeding both is safe.

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{make_atom, try_make_small, AtomTable, Eterm, NIL};

use crate::error;
use crate::runtime::Runtime;

mod dist;
mod host;
mod limits;
mod memory;
mod schedulers;
mod system;
mod time;
mod unsupported;

// The host-identity helpers (`os_type`, `system_architecture`) live in their own child
// module — they are the only keys here that turn a *host* fact into a value, and the
// mapping is worth unit-testing on its own. Re-exported so the `system` family's
// `use super::{os_type, system_architecture, InfoValue};` keeps resolving.
pub(crate) use host::{os_type, system_architecture};

/// The keys [`system_info_value`] answers — every key whose value this runtime can give
/// in the oracle's exact shape. Some are constants of this build/host, others are
/// **self-derived** from this runtime's own tables (see the module docs).
///
/// The list holds **atom** keys only; the two tuple keys the oracle accepts that this
/// runtime answers, `{allocator, A}` and `{wordsize, internal|external}`, go through
/// [`tuple_key_value`] (reached from [`crate::native`]'s `system_info/1` tuple path).
pub const SUPPORTED_KEYS: &[&str] = &[
    // -- `system`: build/host identity (see [`system`]) --
    "version",
    "otp_release",
    "system_architecture",
    "system_version",
    "nif_version",
    "driver_version",
    "wordsize",
    "emu_flavor",
    "emu_type",
    "machine",
    "threads",
    "smp_support",
    "async_dist",
    "os_type",
    "os_version",
    "break_ignored",
    "debug_compiled",
    "dynamic_trace",
    "dynamic_trace_probes",
    "modified_timing_level",
    "update_cpu_info",
    "trace_control_word",
    "port_parallelism",
    "kernel_poll",
    "halt_flush_timeout",
    "creation",
    // The system logger's process — **this runtime's own registration** (the pid the real
    // `start.boot` leaves behind), never a copied oracle pid (see [`system`]).
    "system_logger",
    // The emulator's own build/implementation flags (see [`system`]).
    "compat_rel",
    "endian",
    "eager_check_io",
    "io_thread",
    "port_tasks",
    "lock_checking",
    "lock_counting",
    "ets_always_compress",
    "ets_realloc_moves",
    "hipe_architecture",
    "literal_test",
    "sequential_tracer",
    // -- `limits`: counts and emulator limits (see [`limits`]) --
    "atom_count",
    "atom_limit",
    "process_count",
    "process_limit",
    "port_count",
    "port_limit",
    "ets_count",
    "ets_limit",
    "outstanding_system_requests_limit",
    "dist_buf_busy_limit",
    "delayed_node_table_gc",
    // -- `schedulers`: processor/scheduler keys (see [`schedulers`]) --
    "logical_processors",
    "logical_processors_available",
    "logical_processors_online",
    "schedulers",
    "schedulers_online",
    "scheduler_id",
    "dirty_cpu_schedulers",
    "dirty_cpu_schedulers_online",
    "dirty_io_schedulers",
    "scheduler_bind_type",
    "scheduler_bindings",
    "multi_scheduling",
    "multi_scheduling_blockers",
    "normal_multi_scheduling_blockers",
    "thread_pool_size",
    "context_reductions",
    // The scheduler *state*/statistics keys (see [`schedulers`]).
    "schedulers_total",
    "schedulers_active",
    "schedulers_state",
    "all_schedulers_state",
    "run_queues",
    "cpu_quota",
    "scheduling_statistics",
    "total_scheduling_statistics",
    // -- `memory`: GC/heap + allocator keys (see [`memory`]) --
    "min_heap_size",
    "min_bin_vheap_size",
    "max_heap_size",
    "fullsweep_after",
    "heap_sizes",
    "heap_type",
    "message_queue_data",
    "garbage_collection",
    "alloc_util_allocators",
    "allocated_areas",
    // -- `dist`: the distribution-state keys (see [`dist`]) --
    "dist",
    "dist_ctrl",
    // -- `time`: the ERTS monotonic-time interface constants (see [`time`]) --
    "start_time",
    "end_time",
];

/// The keys the oracle accepts but this runtime answers with an honest **capability
/// gap** — a raise whose reason atom starts with `unsupported`.
///
/// Every key here names an emulator facility this runtime does not have (see the
/// [`unsupported`] module for the per-key rationale). The value is *never* fabricated:
/// reporting an oracle number for a facility that does not exist would be a lie about a
/// different emulator.
pub const UNSUPPORTED_KEYS: &[&str] = &[
    // The emulator's own build tag. It shares the C arm with `emu_type`
    // (`erl_bif_info.c:2800`, `am_build_type || am_emu_type`), but this runtime's own build
    // tier (`debug` in a dev build, `opt` in a release build) is **not** the oracle's
    // release tag (`opt`), so answering it would be an own-state divergence — the honest
    // typed gap instead.
    "build_type",
    // The host C allocator identification (`{glibc, …}`) — no C allocator to identify.
    "allocator",
    // The time-correction service and its clocks.
    "time_offset",
    "time_correction",
    "time_warp_mode",
    "tolerant_timeofday",
    "os_monotonic_time_source",
    "os_system_time_source",
    // CPU-topology *detection*, poll-set introspection, C-compiler/3PP identification.
    "cpu_topology",
    "check_io",
    "c_compiler_used",
    "embedded_3pps",
    // The code-loading self-description dumps.
    "info",
    "procs",
    "loaded",
    // ERTS *C-emulator internals* with no counterpart in this runtime: the C toolchain's
    // build flags, the ethread library, the constant-pool/jump-table loader switches, the
    // static NIF table and its taints, the emulator argument vector, the scheduler
    // distribution maps, the `BIG_ARITY_MAX` bignum cap and the thread-progress debug dump.
    "compile_info",
    "ethread_info",
    "ethread_used_tse",
    "constant_pool_support",
    "beam_jump_table",
    "snifs",
    "taints",
    "emu_args",
    "reader_groups_map",
    "decentralized_counter_groups_map",
    "max_integer",
    "thread_progress",
];

/// The **complete** atom vocabulary of this surface — every name [`system_info_value`]
/// can answer as an [`InfoValue::Atom`] (or nested inside one of its list/tuple/map
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
    // `os_type`: the `{unix,linux}` / `{win32,nt}` pair of the two families (see [`host`]),
    // the other ERTS OS names a supported platform answers (`{unix,darwin}` on macOS,
    // `{unix,freebsd}`/`openbsd`/`netbsd`/`dragonfly` on the BSDs), and
    // `threads`/`*_support` (`true` / `false`).
    "unix", "linux", "win32", "nt", "true", "false",
    "darwin", "freebsd", "openbsd", "netbsd", "dragonfly",
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
    // The `max_heap_size` process-flag map (`process_info(Pid, garbage_collection)`) and
    // the `system_info(max_heap_size)` map, in the oracle's key order.
    "error_logger",
    "include_shared_binaries",
    "kill",
    "size",
    // The remaining `process_info/2` items and the reply for a pid that is not live.
    "heap_size",
    "total_heap_size",
    "undefined",
    // `emu_type` (`opt` for an optimized build, `debug` for a debug build) and the
    // constant atom-valued keys: `dynamic_trace` (`none`), `modified_timing_level`
    // (`undefined`), `update_cpu_info` (`unchanged`), `halt_flush_timeout` (`infinity`),
    // `scheduler_bind_type` (`unbound`), `multi_scheduling` (`enabled`/`disabled`),
    // `heap_type` (`private`) and `message_queue_data` (`on_heap`).
    "opt",
    "debug",
    "none",
    "unchanged",
    "infinity",
    "unbound",
    "enabled",
    "disabled",
    "private",
    "on_heap",
    // `endian` (`little`/`big`, the C's `WORDS_BIGENDIAN`) and `literal_test` (`tag` —
    // this runtime's literal-area test is a tagged-pointer bit, the C's own word for a
    // tag-based test; `range`/`range_bitmask` are the range-check variants).
    "little",
    "big",
    "tag",
    // `sequential_tracer`: the two atoms of `{sequential_tracer,false}` — the system
    // sequential tracer is the disabled atom `false` when none is installed.
    "sequential_tracer",
    // `all_schedulers_state`: the pool names of the `{Pool,Total,Online,Active}` rows
    // (`am_normal`/`am_dirty_cpu`/`am_dirty_io`, `erl_bif_info.c:3302-3328`).
    "normal",
    "dirty_cpu",
    "dirty_io",
    // `scheduling_statistics`/`total_scheduling_statistics`: the five priority-level names
    // of the `{Prio,Executed,Migrated}` rows, in the oracle's order
    // (`erts_sched_stat_term` over `ERTS_NO_PRIO_LEVELS`).
    "process_max",
    "process_high",
    "process_normal",
    "process_low",
    "port",
];

/// A `system_info/1` value, independent of any heap.
///
/// The scalar variants (`Int`, `Str`, `Atom`, `Bool`, `Tuple`, `Binary` and the pid-valued
/// `Pid`); [`InfoValue::List`],
/// [`InfoValue::Tupled`] and [`InfoValue::Map`] are the generic containers the richer
/// keys (`alloc_util_allocators`, `allocated_areas`, `{allocator, A}`,
/// `garbage_collection`, `max_heap_size`) are built from — a `{Key, Value}` row is
/// `Tupled(vec![Atom(key), value])`, so the oracle's proplists and its mixed-arity
/// tuples (`{atom_space, Bytes, Bytes}`) are both expressible, and `Map` covers the one
/// map-shaped key (`max_heap_size`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoValue {
    /// A 64-bit signed integer.
    ///
    /// Materialized as a small-int immediate when it fits the 60-bit small range and as a
    /// one-digit bignum otherwise — exactly the C's `make_time_val`
    /// (`erl_time_sup.c:2133-2146`: `IS_SSMALL(val)` → `make_small`, else
    /// `erts_sint64_to_big`), which is how the oracle answers `system_info(end_time)`
    /// (a value outside the small range).
    Int(i64),
    /// A string (materialized as a list of byte-sized small integers, as the oracle does).
    Str(String),
    /// An atom, by name (materialized through the runtime's global atom table).
    Atom(&'static str),
    /// A boolean (materialized as the atom `true`/`false`).
    Bool(bool),
    /// A two-element tuple of atoms, by name.
    Tuple(&'static str, &'static str),
    /// A byte string (materialized as an on-heap binary).
    ///
    /// Distinct from [`InfoValue::Str`], which is an Erlang **string** — a list of
    /// byte-sized small integers. `Binary` is the shape of `system_info(dist)` (the
    /// oracle's `=node:…`/`=no_distribution` dump), built through the safe heap writer
    /// [`beam_codec::term::make_binary`] so this crate stays `unsafe`-free.
    Binary(Vec<u8>),
    /// A **pid**, materialized as the internal-pid immediate the process table holds.
    ///
    /// The only pid-valued key is `system_logger` (see [`system`]). A [`ProcessId`] encodes
    /// to its term with no heap at all ([`ProcessId::to_eterm`]), so the value is
    /// byte-identical to what `erlang:whereis/1` answers for the same process — the
    /// oracle's own route to `system_logger`'s pid.
    Pid(ProcessId),
    /// A proper list, in order (materialized element by element, head first).
    List(Vec<InfoValue>),
    /// A tuple of arbitrary arity, in order.
    Tupled(Vec<InfoValue>),
    /// A map of `(key, value)` pairs, materialized as an Erlang map.
    ///
    /// The only map-shaped key is `max_heap_size`; it is a small flat map, built through
    /// [`beam_codec::term::make_flatmap`] (the safe heap writer) so this crate stays
    /// `unsafe`-free.
    Map(Vec<(InfoValue, InfoValue)>),
}

impl InfoValue {
    /// Materializes this value as a term on `heap`, resolving atoms through `atoms`.
    ///
    /// # Errors
    ///
    /// * [`BeamError::TermNotFound`] when an atom this value names is not interned in
    ///   `atoms` — never an invented atom. A runtime materializing this crate's keys
    ///   seeds [`INFO_ATOMS`], which covers every atom the surface can emit;
    /// * the heap's allocation failure for a string, a list, a tuple, a map, a binary or
    ///   the block of an [`InfoValue::Int`] outside the small range.
    pub fn to_eterm(&self, heap: &mut Heap, atoms: &AtomTable) -> Result<Eterm> {
        match self {
            InfoValue::Int(value) => integer_term(heap, *value),
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
            InfoValue::Binary(bytes) => beam_codec::term::make_binary(heap, bytes),
            InfoValue::Pid(pid) => Ok(pid.to_eterm()),
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
            InfoValue::Map(pairs) => {
                let mut terms = Vec::with_capacity(pairs.len());
                for (key, value) in pairs {
                    terms.push((key.to_eterm(heap, atoms)?, value.to_eterm(heap, atoms)?));
                }
                beam_codec::term::make_flatmap(heap, &terms)
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

/// A 64-bit signed integer as a term: a small-int immediate when it fits the 60-bit small
/// range, else a one-digit bignum.
///
/// The C's `make_time_val` (`erl_time_sup.c:2133-2146`) picks between exactly these two
/// representations (`IS_SSMALL(val)` → `make_small`, else `erts_sint64_to_big`), and
/// `erts_sint64_to_big` writes one `ErtsDigit` on `ARCH_64` (`big.c:2357-2364`,
/// `big.h:120-121`), which is what [`beam_codec::term::make_big`] writes for a single
/// digit. The magnitude comes from `unsigned_abs`, so `i64::MIN` cannot overflow.
fn integer_term(heap: &mut Heap, value: i64) -> Result<Eterm> {
    match try_make_small(value) {
        Ok(small) => Ok(small),
        Err(_) => beam_codec::term::make_big(heap, value < 0, &[value.unsigned_abs()]),
    }
}

/// Computes the `system_info/1` value of `key` against a runtime.
///
/// # Errors
///
/// * [`BeamError::BadArg`] for any key outside [`SUPPORTED_KEYS`]/[`UNSUPPORTED_KEYS`] —
///   the oracle's reason for a key it does not accept;
/// * the [`crate::error::unsupported`] capability gap for a key of [`UNSUPPORTED_KEYS`]
///   (a facility this runtime does not have);
/// * the materialization errors of [`allocator_report`] and the live-table reads.
pub fn system_info_value(key: &str, rt: &Runtime) -> Result<InfoValue> {
    if let Some(result) = system::try_value(key, rt) {
        return result;
    }
    if let Some(result) = limits::try_value(key, rt) {
        return result;
    }
    if let Some(result) = schedulers::try_value(key, rt) {
        return result;
    }
    if let Some(result) = memory::try_value(key, rt) {
        return result;
    }
    if let Some(result) = dist::try_value(key, rt) {
        return result;
    }
    if let Some(result) = time::try_value(key, rt) {
        return result;
    }
    if let Some(result) = unsupported::try_value(key, rt) {
        return result;
    }
    Err(error::unknown_info_key(key))
}

/// The value of a **tuple** `system_info/1` key, or the oracle's `badarg` when neither
/// the tag nor the argument is one this runtime answers.
///
/// The oracle dispatches tuple keys on the tag *and* the argument
/// (`erl_bif_info.c:2521-2527` for `{wordsize, W}`, `erl_bif_info.c:2529-2546` for
/// `{allocator, A}`); everything else is `badarg`. The two answered here are:
///
/// * `{allocator, A}` → [`allocator_report`] (the measured `beam_alloc` counters);
/// * `{wordsize, W}` → this machine's word size in bytes, `internal` being one term word
///   and `external` one `UWord` ([`system::wordsize_pair`]).
///
/// # Errors
///
/// [`BeamError::BadArg`] for an unknown tag or an unknown argument — never an invented
/// answer.
pub fn tuple_key_value(tag: &str, arg: &str) -> Result<InfoValue> {
    match tag {
        "allocator" => allocator_report(arg),
        "wordsize" => system::wordsize_pair(arg),
        _ => Err(BeamError::BadArg),
    }
}

// -- shared helpers used by the family modules ----------------------------------------

/// A `{Key, Value}` row of an `InfoValue` proplist.
pub(crate) fn row(key: &'static str, value: i64) -> InfoValue {
    InfoValue::Tupled(vec![InfoValue::Atom(key), InfoValue::Int(value)])
}

/// `value` as an `i64`, saturating at the small-integer range's practical bound.
///
/// Used only for footprint/count figures where the operand is far inside the range on
/// any real machine (a table length, a size in bytes, the growth table).
pub(crate) fn usize_to_int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `value` as an [`InfoValue::Int`], or [`BeamError::SystemLimit`] when it cannot be a
/// small integer.
pub(crate) fn usize_to_int_result(value: usize) -> Result<InfoValue> {
    i64::try_from(value)
        .map(InfoValue::Int)
        .map_err(|_| BeamError::SystemLimit)
}

/// The number of logical processors, best-effort.
///
/// `system_info/1` must always answer, so a platform that cannot report the count
/// (rather than an error) falls back to `1` — documented rather than swallowed.
pub(crate) fn logical_processors() -> i64 {
    match std::thread::available_parallelism() {
        Ok(count) => count.get() as i64,
        Err(_) => 1,
    }
}

/// The number of **normal schedulers**, as the oracle reports it.
///
/// An explicit `+S N` wins. `+S 0` means "auto" in real ERTS (`erl +S 0` reports the
/// online processor count), so it falls through to the machine's parallelism as well —
/// the same answer the oracle gives without `+S`. The count is always `> 0`: the
/// scheduler *loop* is Stage-5 work (`beam_process`), but a count of `0` schedulers
/// would be a lie about a runtime that executes code.
pub(crate) fn schedulers_count(rt: &Runtime) -> i64 {
    match rt.config().emulator.schedulers {
        Some(count) if count > 0 => i64::from(count),
        _ => logical_processors(),
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
pub(crate) fn live_ports(rt: &Runtime) -> Result<i64> {
    match rt.ports.try_borrow() {
        Ok(registry) => Ok(registry.len() as i64),
        Err(_) => Err(error::unsupported(
            "erlang:system_info(port_count): the port registry is already borrowed",
        )),
    }
}

/// The number of live ETS tables, or the typed capability gap when the shared table
/// registry is already borrowed.
pub(crate) fn live_tables(rt: &Runtime) -> Result<i64> {
    match rt.tables.try_borrow() {
        Ok(tables) => Ok(tables.len() as i64),
        Err(_) => Err(error::unsupported(
            "erlang:system_info(ets_count): the ETS table registry is already borrowed",
        )),
    }
}

/// The **effective** heap policy this runtime reports: the emulator's heap-size options
/// (`+hms`/`+hmbs`/`+hmax*`) as parsed by the CLI.
///
/// [`beam_gc::HeapPolicy::default`] is the *unbounded floor* (`min_heap_words == 0`),
/// whereas the oracle reports the C's `H_DEFAULT_SIZE` (233 words); `beam_gc`'s
/// [`HeapPolicy::from_spawn_opts`] is the spawn path that applies that floor.
/// [`HeapOptions`](crate::config::HeapOptions) already carries the settled emulator
/// figures — the defaults are 233 / 46422 / 65535 and a `+hms 1000` / `+hmbs 5000`
/// invocation reports 1598 / 6772 — all computed from `beam_gc`'s constants and growth
/// table, never hardcoded from the oracle. Reporting it (rather than re-deriving it from
/// a collector's policy) matches the C, where `min_heap_size`/`min_bin_vheap_size` are
/// seeded per process from the emulator defaults.
pub(crate) fn effective_policy(rt: &Runtime) -> beam_gc::HeapPolicy {
    rt.config().heap.policy().clone()
}

// -- `{allocator, A}` -----------------------------------------------------------------

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

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the `info` family modules' unit tests.
    use crate::config::RuntimeConfig;
    use crate::runtime::Runtime;

    /// A default runtime (`sleep`-free; nothing is executed).
    pub(crate) fn runtime() -> Runtime {
        Runtime::new(RuntimeConfig::default()).expect("the default runtime")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_bifs::BifRegistry;
    use beam_common::OTP_VERSION;
    use beam_process::ProcessId;
    use beam_term::{atom_val, eq, is_atom, is_small, signed_value};

    use crate::config::RuntimeConfig;
    use crate::runtime::Runtime;

    fn runtime() -> Runtime {
        Runtime::new(RuntimeConfig::default()).expect("the default runtime")
    }

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        crate::boot::seed_core_atoms(&mut atoms);
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
        // The surface is the union of the family modules; keep the count pinned so a key
        // dropped from a family is caught.
        assert_eq!(SUPPORTED_KEYS.len(), 88);
        let mut sorted: Vec<&str> = SUPPORTED_KEYS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), SUPPORTED_KEYS.len(), "no duplicate keys");
        for key in UNSUPPORTED_KEYS {
            assert!(!SUPPORTED_KEYS.contains(key), "{key} cannot be both");
        }
    }

    #[test]
    fn every_unsupported_key_reports_a_capability_gap() {
        let rt = runtime();
        for key in UNSUPPORTED_KEYS {
            let error = system_info_value(key, &rt)
                .expect_err(&format!("system_info({key}) must be a capability gap"));
            assert!(
                crate::error::unsupported_reason(&error).is_some(),
                "system_info({key}) must classify as unsupported, got {error:?}"
            );
        }
        let mut sorted: Vec<&str> = UNSUPPORTED_KEYS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), UNSUPPORTED_KEYS.len(), "no duplicate keys");
        assert_eq!(sorted.len(), 27);
    }

    #[test]
    fn every_supported_key_materializes_once_the_info_atoms_are_seeded() {
        // `INFO_ATOMS` is the whole contract the introspection surface has with the atom
        // table: seeding exactly this list must materialize **every** supported key (the
        // oracle's `system_info/1` never returns an unrepresentable value).
        let rt = runtime();
        let mut heap = Heap::new(8_192).expect("test heap");
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
    fn oracle_rejected_keys_stay_badarg() {
        // The oracle raises `error:badarg` for these (verified against OTP 29.1.1 /
        // erts-17.1), so this runtime must not invent an answer for them.
        let rt = runtime();
        for key in [
            "allocator_sizes",
            "beam_jit_available",
            "c_library_version",
            "export_count",
            "export_limit",
            "module_count",
            "module_limit",
            "net_ticktime",
            "node",
            "pure",
            "busy_limits",
            "catchlevel",
            "safe_string_hash",
            "memory",
            "run_queue",
            "total_run_queue_lengths",
            "wall_clock",
        ] {
            assert!(
                matches!(system_info_value(key, &rt), Err(BeamError::BadArg)),
                "system_info({key}) must be badarg"
            );
        }
    }

    #[test]
    fn to_eterm_materializes_every_variant() {
        let mut heap = Heap::new(512).expect("test heap");
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

        // A byte string materializes as a **binary** carrying exactly those bytes (this is
        // the shape of `system_info(dist)`), not as an Erlang string.
        let payload: &[u8] = b"=node:'nonode@nohost'\n=no_distribution\n";
        let binary = InfoValue::Binary(payload.to_vec())
            .to_eterm(&mut heap, &atoms)
            .expect("binary");
        let (bytes, offset_bits, size_bits) =
            beam_codec::term::bitstring_bytes(binary).expect("a bitstring");
        assert_eq!(offset_bits, 0);
        assert_eq!(size_bits, payload.len() * 8, "byte-aligned: a binary");
        assert_eq!(&bytes[..size_bits / 8], payload);
        // No atom table is needed — a binary carries no atom.
        assert!(InfoValue::Binary(payload.to_vec())
            .to_eterm(&mut heap, &AtomTable::new())
            .is_ok());
    }

    #[test]
    fn the_pid_variant_materializes_to_a_real_pid_without_a_heap_or_atom_table() {
        // `system_logger` is the one pid-valued key; its term must be exactly what
        // `erlang:whereis/1` answers for that process, i.e. the internal-pid immediate.
        let mut heap = Heap::new(64).expect("test heap");
        let pid = ProcessId::new(7, 1);
        let term = InfoValue::Pid(pid)
            .to_eterm(&mut heap, &seeded_atoms())
            .expect("pid");
        assert_eq!(term, pid.to_eterm());
        assert!(ProcessId::is_process(term));
        // A pid is an immediate: it needs no heap space and resolves no atom.
        assert!(InfoValue::Pid(pid)
            .to_eterm(&mut heap, &AtomTable::new())
            .is_ok());
    }

    #[test]
    fn system_logger_reports_the_registered_logger_process_or_undefined() {
        let mut rt = runtime();
        // A runtime that never ran a boot script has no process registered as `logger`, so
        // the honest answer is the atom `undefined` — one of the two non-pid shapes the
        // oracle itself produces for this key
        // (`erlang:system_flag(system_logger, undefined)`, `logger_proxy.erl:147`).
        assert_eq!(
            system_info_value("system_logger", &rt).expect("system_logger"),
            InfoValue::Atom("undefined")
        );
        // A process registered as `logger` — what the real `start.boot` leaves behind
        // (instruction 13's `{kernelProcess,logger,{logger_server,start_link,[]}}`) — is
        // reported as its pid, never a fabricated one.
        let pid = rt.self_pid();
        rt.register_boot_process("logger", pid)
            .expect("`logger` is not yet registered");
        let value = system_info_value("system_logger", &rt).expect("system_logger");
        assert_eq!(value, InfoValue::Pid(pid));
        let mut heap = Heap::new(64).expect("test heap");
        assert_eq!(
            value
                .to_eterm(&mut heap, &seeded_atoms())
                .expect("a pid needs no heap or atom"),
            pid.to_eterm(),
            "the value is byte-identical to `erlang:whereis(logger)`"
        );
    }

    #[test]
    fn the_map_variant_materializes_to_a_real_map() {
        let mut heap = Heap::new(512).expect("test heap");
        let atoms = seeded_atoms();
        let value = InfoValue::Map(vec![
            (InfoValue::Atom("size"), InfoValue::Int(0)),
            (InfoValue::Atom("kill"), InfoValue::Bool(true)),
        ]);
        let term = value.to_eterm(&mut heap, &atoms).expect("a map");
        let entries = beam_engine::terms::map_entries(term).expect("a map term");
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|(k, v)| atoms.name(atom_val(*k)) == Some("size") && *v == beam_term::make_small(0)));
        assert!(entries
            .iter()
            .any(|(k, v)| atoms.name(atom_val(*k)) == Some("kill") && is_atom(*v)));
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
        assert!(matches!(
            InfoValue::Map(vec![(InfoValue::Atom("unix"), InfoValue::Int(1))])
                .to_eterm(&mut heap, &atoms),
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
        let unseeded = AtomTable::new();
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
