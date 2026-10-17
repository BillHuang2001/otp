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

//! `erlang:process_info/1,2` — the per-process introspection surface (contract A.8).
//!
//! Like the rest of the floor ([`super`]), this is a *runtime* operation: the answer is
//! read out of this runtime's own process table, so it is served here, **before** the
//! `beam_bifs` registry is consulted.
//!
//! # The two shapes the oracle answers
//!
//! `erlang:process_info/2` accepts an **item atom**, a **list of items** or the 2-tuple
//! `{dictionary, Key}`:
//!
//! * `process_info(Pid, Item)` → `{Item, Value}` — except `registered_name`, whose value is
//!   the **bare** name atom, and the bare `[]` when the process is **unregistered**. The C's
//!   `always_wrap = !is_atom(opt)` (`erl_bif_info.c:1556`) is what makes that one item
//!   unwrapped in the single-atom form.
//! * `process_info(Pid, [Item, …])` → `[{Item, Value}, …]`, in the requested order, with
//!   duplicates repeated. Here `registered_name` **is** wrapped in both cases:
//!   `{registered_name, Name}` or `{registered_name, []}`.
//! * `process_info(Pid, {dictionary, Key})` → `{{dictionary, Key}, Value|undefined}`.
//!
//! `erlang:process_info/1` answers the **fixed** key set of the C's `pi_1_keys[]`
//! (`erl_bif_info.c:1008-1026`), in that exact order. For a **registered** process it emits
//! all 17 items, `{registered_name, Name}` first; for an **unregistered** one it **omits**
//! `registered_name` and emits the remaining 16. (The C skips it because `always_wrap = 0`
//! makes an unregistered `registered_name` the bare `[]`, which the multi-item loop treats as
//! "no value": `erl_bif_info.c:1175`.) An item this runtime cannot answer is **omitted** —
//! the very mechanism the C uses to drop an unregistered `registered_name` — so a process
//! whose execution position this runtime cannot read omits the two engine-derived keys
//! `pi_1_keys[]` asks for; only a process this runtime never gave a position (an
//! unscheduled pid, a fun-bodied process with no resolved target) is such a process today
//! (see [`RuntimeContext::position_value`]): the caller's own position **is** read from its
//! live machine by the in-machine BIF door, and a process the runtime scheduled reports all
//! 17.
//!
//! # The failure modes (verified against OTP 29.1.1 / erts-17.1)
//!
//! * a non-atom, non-list, non-`{dictionary,_}` second argument ⇒ `error:badarg`;
//! * an **unknown item name** (or an unknown item *inside* a list, or an improper list) ⇒
//!   `error:badarg`, **even** for a pid that is not live;
//! * a first argument that is not an internal pid ⇒ `error:badarg`;
//! * a valid item with a valid pid that is **not in the process table** ⇒ the atom
//!   `undefined` (for the single form, the list form and `process_info/1` alike — the list
//!   form of a dead pid is `undefined`, *not* `[]`).
//!
//! # Items this runtime answers
//!
//! | item | shape |
//! |---|---|
//! | `registered_name` | `{registered_name, Name}` when registered; the bare `[]` (unregistered) in the `/2`-atom form, `{registered_name, []}` in the list form, omitted by `/1` |
//! | `status` | `running`/`runnable`/`waiting`/`suspended`/`exiting` (or the transient `garbage_collecting`) from [`Process::status`](beam_process::Process::status) |
//! | `message_queue_len` | the mailbox length |
//! | `messages` | the queued payload terms, FIFO |
//! | `links` | the linked pids |
//! | `monitors` | `[{process, Pid} \| {port, Port}]` — the outgoing process monitors followed by the outgoing port monitors (see [`Snapshot::monitors`]) |
//! | `monitored_by` | the watching pids |
//! | `dictionary` | the `{Key, Value}` pairs, insertion order |
//! | `{dictionary, Key}` | the stored value, else `undefined` |
//! | `trap_exit` | the `trap_exit` flag |
//! | `error_handler` | the atom `error_handler` (an unchanging runtime default) |
//! | `priority` | the scheduling priority atom |
//! | `group_leader` | this runtime's root pid (what `erlang:group_leader/0` answers) |
//! | `current_function` / `initial_call` | `{M, F, A}` of the function the process is in / was entered at — from the engine, see below |
//! | `current_location` | `{M, F, A, [{file, File}, {line, L}]}` |
//! | `current_stacktrace` | `[{M, F, A, Location}, …]`, innermost frame first |
//! | `catchlevel` | the live exception catch-frame depth, as an integer — from the engine, see below |
//! | `total_heap_size` / `heap_size` | this process's real heap capacities, in words |
//! | `stack_size` | the Erlang stack's word count |
//! | `reductions` | this process's real reduction counter |
//! | `garbage_collection` | the oracle's five-item proplist (see below) |
//! | `min_heap_size` / `min_bin_vheap_size` / `fullsweep_after` | the emulator's heap policy |
//! | `max_heap_size` | the process flag **map** |
//! | `message_queue_data` | `on_heap` — this runtime's real *current* placement (the queue is heap-resident; see the divergence note below) |
//! | `trace` | the process's **live** ERTS tracee-flag **bitmask**, as an integer (`0` while it is untraced) — see below |
//! | `sequential_trace_token` | the process's sequential-trace token tuple, or the bare `[]` while tracing is inactive for it (see [`crate::seq_trace`]) |
//! | `suspending` | `[]` (no `suspend_process` in M1) |
//! | `last_calls` | `false` — the oracle's own answer for a process with no saved-calls buffer |
//! | `memory` | this process's real bytes (heap + stack + off-heap) |
//! | `binary` | a list of `{Ptr, Size, Refc}` 3-tuples for the process's off-heap refc binaries (payload pointer as an integer, payload byte length, payload refcount); the empty list `[]` when the process holds none |
//!
//! ## Items this runtime honestly cannot answer (`unsupported`)
//!
//! These are *valid* oracle item names, so answering `badarg` would be a lie. Each is a
//! typed capability gap ([`crate::error::unsupported`], reason atom starting with
//! `unsupported`) naming the precise missing accessor/hook ([`UNSUPPORTED_ITEM_REASONS`]):
//!
//! | item | why / what is needed |
//! |---|---|
//! | `backtrace` | no raw/binary stack-dump accessor — the engine builds only the cooked `current_stacktrace` list, not the C's `erts_stack_dump` binary |
//! | `magic_ref` | no magic ref is bound into a process off-heap table — `beam_abi`'s NIF resource API is unbound and nothing registers a resource with a process |
//! | `parent` | no parent field on the PCB ([`beam_process::Process`]) — the spawn path never records the spawning process |
//! | `label` | no label field on the PCB ([`beam_process::Process`]) |
//! | `priority_messages` | no `priority_messages` flag in [`beam_process::ProcessFlags`] |
//! | `garbage_collection_info` | [`beam_gc::HeapInfo`] tracks no per-generation block detail (the oracle's `old_heap_block_size`/`mbuf_size`/`recent_size`/`bin_vheap_block_size`/`bin_old_vheap_block_size` are unmeasured) |
//!
//! The **execution-position** items (`current_function`, `initial_call`, `current_location`,
//! `current_stacktrace`) and `catchlevel` are therefore *conditionally* answerable rather
//! than gaps: they are built by the engine's read-only [`Machine`](beam_engine::Machine)
//! accessors out of the process's own entry module and engine, which is what
//! [`RuntimeContext::position_value`] / [`RuntimeContext::catchlevel_value`] read — through
//! the [`engines`](RuntimeContext::engines) view of the runner state for *another* process,
//! and through the **live machine** of the executing process for the caller's own
//! `erlang:process_info/1,2` dispatched from **compiled code** (the in-machine BIF door,
//! [`crate::context::position`]; the scheduler takes the executing process's machine *out*
//! of the runner state for the whole slice, so no other source could name it). They are
//! served for a process this runtime scheduled — parked (the case the oracle's values below
//! are pinned against) or, for the caller itself, executing — and are omitted (`/1`) or the
//! typed capability gap (`/2`) for a process with no readable position: a **fun-bodied**
//! process whose local target is unresolved (its entry is a fun value: no module could name
//! `M`/`F`), any pid this runtime never scheduled, and a **direct/no-machine** caller (a
//! library or `-eval` caller that reaches `call_bif` without a `Machine`, so no door is
//! open).
//!
//! ## `trace`
//!
//! The C answers the process's **tracee-flag bitmask** as an *integer* —
//! `res = make_small(ERTS_IS_P_TRACED_FL(rp, TRACEE_FLAGS))` (`erl_bif_info.c:2133`) — never the
//! atom list `erlang:trace_info(Pid, flags)` reports. Verified against the oracle (OTP 29.1.1 /
//! erts-17.1):
//!
//! ```text
//! P = spawn(fun() -> receive stop -> ok end end),
//! process_info(P, trace)                              => {trace,0}
//! erlang:trace(P, true, [call]), process_info(P,trace) => {trace,1024}
//! erlang:trace(P, true, [all]),  process_info(P,trace) => {trace,32763895}
//! ```
//!
//! The value is read out of this runtime's **live** per-process trace state
//! ([`crate::trace::TraceState`] — what `erlang:trace/3` maintains), so a traced process reports
//! its real flag bitmask and an untraced one the zero bitmask. This runtime's *internal* flag
//! bits are deliberately not ERTS's ([`crate::trace`]'s constants are its own), so the
//! translation goes through the public [`crate::trace::flags_to_atoms`] and
//! [`ERTS_TRACE_FLAG_BITS`], the ERTS bit each canonical atom sets
//! (`F_TRACE_FLAG(n) = 1 << (ERTS_TRACE_TS_TYPE_BITS + n)`, `erl_process.h:1760`, with
//! `ERTS_TRACE_TS_TYPE_BITS = 3`, plus the three low timestamp bits). The two flags this runtime
//! carries that have **no** canonical atom (`return_from`, `cpu_timestamp`) are deliberately
//! unmapped: the oracle's own `all` bitmask excludes them and `erlang:trace(P, true, [ThatFlag])`
//! is `badarg` on this oracle.
//!
//! ## `garbage_collection`
//!
//! Verified against the oracle (OTP 29 / erts-17.1) for a live, idle spawned process:
//!
//! ```text
//! {garbage_collection,
//!  [{max_heap_size, #{error_logger => true,include_shared_binaries => false,
//!                    kill => true,size => 0}},
//!   {min_bin_vheap_size,46422},
//!   {min_heap_size,233},
//!   {fullsweep_after,65535},
//!   {minor_gcs,0}]}
//! ```
//!
//! The key order is [`beam_gc::GARBAGE_COLLECTION_KEYS`]; `max_heap_size` is the flag
//! **map** here (`size` is the raw word bound, 0 = unlimited), unlike the bare integer
//! `erlang:system_info(garbage_collection)` reports. `min_heap_size` / `min_bin_vheap_size` /
//! `fullsweep_after` come from the **emulator's** heap-size options — the
//! `+hms`/`+hmbs`/`+hmax*`-configured policy the process was seeded with
//! ([`crate::info::effective_policy`]), so `+hms 1000 +hmbs 5000` reports 1598 / 6772 — and
//! `minor_gcs` is the collector's real counter (today always `0`: this runtime runs no GC).
//! Nothing here is hardcoded from the oracle.
//!
//! # Documented divergences (correct-but-different-by-construction)
//!
//! * `heap_size` / `total_heap_size`: the oracle reports `233` for both on a fresh idle
//!   process. This runtime reports its **real** capacities, so an idle process reports `233`
//!   for `heap_size` (the young space is `initial_young_words` = 233 words) but `466` for
//!   `total_heap_size`, because the collector pre-allocates a 233-word mature generation
//!   where the oracle's old heap is still empty. Reporting the real figure is deliberate:
//!   the number is a property of *this* runtime's heap, and inventing `233` would be a lie.
//! * `memory`: this runtime's own byte total (`(total_heap_size + stack_size) * 8` plus the
//!   off-heap overhead), which is smaller than the oracle's because this heap is smaller.
//! * `status`: this runtime is single-threaded, so the process making the call is `running`
//!   (the oracle's answer for the caller) and every other live process carries its own
//!   [`Process::status`](beam_process::Process::status) (a process our runner has parked in
//!   `receive` is `waiting`, exactly the oracle's answer for a blocked process).
//! * `group_leader` is this runtime's root pid — the same real, live pid
//!   `erlang:group_leader/0` answers — not the oracle's `<0.70.0>` io process.
//! * `reductions` is *this* process's real counter, whose absolute value differs from the
//!   oracle's because the executed code differs.
//! * `message_queue_data` is reported as `on_heap` because that is this runtime's **true current
//!   placement**: [`beam_message::Queue`] is a heap-resident FIFO, so `on_heap` is the real
//!   value and not an arbitrary default. The one divergence is an **explicit**
//!   `process_flag(message_queue_data, off_heap)`, which [`super::process`] accepts and ignores
//!   (honouring it needs per-process storage in `beam_process::Process` — outside this crate),
//!   so such a process reports `on_heap` where the oracle would report `off_heap`.
//! * `registered_name` is the truth of the **queried** process: the name is looked up by the pid
//!   argument, never by the caller, so asking about another process reports that process's name
//!   while the caller's own item keeps its own. `process_info/1`'s item list **omits** the entry
//!   for an unregistered process (the C's `nil` skip), so the `-eval`/`-s`/`-run` action routes
//!   answer `[]` for the caller's own `process_info(self(), registered_name)` — the action runs in
//!   a spawned, unregistered child of `init` (`src/runtime.rs::as_action_process`), as the oracle's
//!   does.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::{HeapInfo, HeapObservable};
use beam_process::{MonitorKind, ProcessId};
use beam_term::{make_small, Atom, Eterm, NIL};

use super::RuntimeContext;
use crate::config::MaxHeapSetting;

mod binary;
/// The C's `pi_1_keys[]` (`erl_bif_info.c:1008-1026`), in order — the item set *and* order
/// `erlang:process_info/1` answers. `registered_name` is skipped when the process is
/// unregistered (the C's `nil` skip), which is why the oracle emits 16 items.
///
/// `pub(crate)` (inside a `pub(crate)` module) so the [`ProcessBridge`]'s unit tests can
/// intern the very same vocabulary into the shared table instead of duplicating the list.
///
/// [`ProcessBridge`]: crate::context::process_bridge::ProcessBridge
pub(crate) const PI_1_KEYS: &[&str] = &[
    "registered_name",
    "current_function",
    "initial_call",
    "status",
    "message_queue_len",
    "links",
    "dictionary",
    "trap_exit",
    "error_handler",
    "priority",
    "group_leader",
    "total_heap_size",
    "heap_size",
    "stack_size",
    "reductions",
    "garbage_collection",
    "suspending",
];

/// The item names this runtime answers with a genuine value from its own state.
const IMPLEMENTED_ITEMS: &[&str] = &[
    "registered_name",
    "status",
    "message_queue_len",
    "messages",
    "links",
    "monitors",
    "monitored_by",
    "dictionary",
    "trap_exit",
    "async_dist",
    "error_handler",
    "priority",
    "group_leader",
    "current_function",
    "initial_call",
    "current_location",
    "current_stacktrace",
    "catchlevel",
    "total_heap_size",
    "heap_size",
    "stack_size",
    "reductions",
    "garbage_collection",
    "min_heap_size",
    "min_bin_vheap_size",
    "max_heap_size",
    "fullsweep_after",
    "message_queue_data",
    "trace",
    "sequential_trace_token",
    "suspending",
    "last_calls",
    "memory",
    "binary",
];

/// The valid oracle item names this runtime cannot yet answer — a typed `unsupported`
/// capability gap rather than a fabricated value or a false `badarg` (see the module docs).
///
/// `catchlevel` is **not** here: it is answerable through the engine (see
/// [`RuntimeContext::catchlevel_value`]).
const UNSUPPORTED_ITEMS: &[&str] = &[
    "backtrace",
    "magic_ref",
    "parent",
    "label",
    "priority_messages",
    "garbage_collection_info",
];

/// Why each item in [`UNSUPPORTED_ITEMS`] cannot be answered — the precise missing
/// accessor/hook, verified against the real backing code. [`RuntimeContext::unsupported_item`]
/// reports this text, so a capability gap names what is actually missing instead of a vague
/// placeholder. An item whose data becomes available must move to [`IMPLEMENTED_ITEMS`] and
/// lose its row here.
const UNSUPPORTED_ITEM_REASONS: &[(&str, &str)] = &[
    (
        "backtrace",
        "no raw/binary stack-dump accessor: the engine exposes only the cooked \
         `Machine::current_stacktrace` list, never the C's `erts_stack_dump` binary",
    ),
    (
        "magic_ref",
        "no magic ref is bound into a process off-heap table: `beam_abi`'s NIF resource API \
         (`enif_alloc_resource`/`enif_make_resource`) is unbound and nothing registers a \
         resource with a process",
    ),
    (
        "parent",
        "no parent field on the PCB (`beam_process::Process`): the spawn path \
         (`Runtime::spawn_process`/`spawn_fun_process`) never records the spawning process",
    ),
    (
        "label",
        "no label field on the PCB (`beam_process::Process`)",
    ),
    (
        "priority_messages",
        "no priority_messages flag in `beam_process::ProcessFlags`",
    ),
    (
        "garbage_collection_info",
        "`beam_gc::HeapInfo` tracks no per-generation block detail: the oracle's \
         `old_heap_block_size`/`mbuf_size`/`recent_size`/`bin_vheap_block_size`/\
         `bin_old_vheap_block_size` are not measured by the collector",
    ),
];

/// One parsed `process_info` item: an atom name, or the `{dictionary, Key}` 2-tuple.
enum Item {
    /// An item atom.
    Atom(String),
    /// `{dictionary, Key}`.
    DictionaryKey(Eterm),
}

/// The **execution-position** items — the ones served from the engine's read-only
/// `Machine` accessors rather than from the runtime's own counters (see
/// [`RuntimeContext::position_value`]).
const POSITION_ITEMS: &[&str] = &[
    "current_function",
    "initial_call",
    "current_location",
    "current_stacktrace",
];

/// The **ERTS** tracee-flag bit each canonical trace-flag atom sets — the `trace` item's
/// vocabulary (`F_TRACE_FLAG(n) = 1 << (ERTS_TRACE_TS_TYPE_BITS + n)` with
/// `ERTS_TRACE_TS_TYPE_BITS = 3`, `erl_process.h:1760`, plus the three low timestamp bits,
/// `erl_trace.h:32-34`).
///
/// The bits are pinned to the oracle's own `erlang:process_info(P, trace)` answers, one isolated
/// `erlang:trace(P, true, [Atom])` per row (verified on OTP 29.1.1 / erts-17.1); the 21 rows sum
/// to `32763895`, exactly the oracle's `[all]` bitmask. The two flags this runtime carries with
/// **no** canonical atom (`return_from`, `cpu_timestamp`) are absent here on purpose: they are
/// not part of the oracle's `all` and `erlang:trace(P, true, [ThatFlag])` is `badarg` there.
///
/// The one-atom-per-row values are pinned by
/// `trace_bitmask_maps_every_canonical_atom_to_the_oracles_bit` in `tests.rs`, which also fails
/// if [`crate::trace::flags_to_atoms`] ever reports an atom this table lacks (the drift guard
/// the table's coverage relies on).
const ERTS_TRACE_FLAG_BITS: &[(&str, i64)] = &[
    ("timestamp", 1 << 0),                  // 1
    ("strict_monotonic_timestamp", 1 << 1), // 2
    ("monotonic_timestamp", 1 << 2),        // 4
    ("send", 1 << 4),                       // 16
    ("receive", 1 << 5),                    // 32
    ("set_on_spawn", 1 << 6),               // 64
    ("set_on_first_spawn", 1 << 7),         // 128
    ("set_on_link", 1 << 8),                // 256
    ("set_on_first_link", 1 << 9),          // 512
    ("call", 1 << 10),                      // 1024
    ("procs", 1 << 11),                     // 2048
    ("running", 1 << 13),                   // 8192
    ("garbage_collection", 1 << 14),        // 16384
    ("arity", 1 << 15),                     // 32768
    ("return_to", 1 << 16),                 // 65536
    ("silent", 1 << 17),                    // 131072
    ("running_ports", 1 << 20),             // 1048576
    ("running_procs", 1 << 21),             // 2097152
    ("ports", 1 << 22),                     // 4194304
    ("scheduler_id", 1 << 23),              // 8388608
    ("exiting", 1 << 24),                   // 16777216
];

/// This runtime's per-process trace flags as the **ERTS** tracee-flag bitmask the `trace` item
/// reports.
///
/// [`crate::trace`]'s internal bits are its own (its `CANONICAL` order is not ERTS's and two of
/// its flags have no canonical atom), so the translation goes through the public
/// [`crate::trace::flags_to_atoms`] and [`ERTS_TRACE_FLAG_BITS`] rather than reading any bit
/// directly. An untraced process has [`TraceFlags::default`] (no atoms) ⇒ `0`, the oracle's
/// answer.
///
/// [`TraceFlags::default`]: crate::trace::TraceFlags::default
fn trace_bitmask(flags: crate::trace::TraceFlags) -> i64 {
    let atoms = crate::trace::flags_to_atoms(flags);
    let mut bits: i64 = 0;
    for (name, bit) in ERTS_TRACE_FLAG_BITS {
        if atoms.contains(name) {
            bits |= bit;
        }
    }
    bits
}

impl Item {
    /// The term the reply's `{Item, Value}` pair uses as its first element.
    fn key(&self, ctx: &mut RuntimeContext<'_>) -> Result<Eterm> {
        match self {
            Item::Atom(name) => ctx.atom(name),
            Item::DictionaryKey(key) => {
                let tag = ctx.atom("dictionary")?;
                terms::make_tuple(ctx, &[tag, *key])
            }
        }
    }
}

/// One process's observables, copied out of the process table.
///
/// Copied rather than borrowed because the reply is built on the engine's heap afterwards,
/// which needs `&mut` access to the runtime the process table lives in.
struct Snapshot {
    /// The pid these observables belong to (the execution-position items need it).
    pid: ProcessId,
    /// The registered name, `None` when unregistered.
    registered_name: Option<String>,
    /// The scheduler state, as its oracle atom.
    status: &'static str,
    /// The mailbox length.
    message_queue_len: usize,
    /// The queued payloads, FIFO.
    messages: Vec<Eterm>,
    /// The linked pids.
    links: Vec<Eterm>,
    /// The outgoing monitors: the process monitors (`Process::monitors`) followed by the
    /// port monitors (`Process::port_monitors`), each as `(is_a_process_monitor, target)`
    /// where a port monitor's target is the port id term.
    monitors: Vec<(bool, Eterm)>,
    /// The watching pids.
    monitored_by: Vec<Eterm>,
    /// The process dictionary pairs, insertion order.
    dictionary: Vec<(Eterm, Eterm)>,
    /// The `trap_exit` flag.
    trap_exit: bool,
    /// The scheduling priority atom.
    priority: &'static str,
    /// The group leader (this runtime's root pid).
    group_leader: Eterm,
    /// The nursery capacity, in words.
    heap_size: usize,
    /// The nursery plus the mature generation, in words.
    total_heap_size: usize,
    /// The Erlang stack's word count.
    stack_size: usize,
    /// The real reduction counter.
    reductions: u64,
    /// Minor collections run so far.
    minor_gcs: u32,
    /// The **target process's own** heap policy — what `spawn_opt`'s `min_heap_size` /
    /// `min_bin_vheap_size` / `fullsweep_after` / `max_heap_size` options set for a child,
    /// and the emulator's default policy for every other process
    /// ([`Runtime::default_spawn_policy`](crate::Runtime::default_spawn_policy)). The heap
    /// items are answered from here, so `process_info(Child, min_heap_size)` is the
    /// *child's* figure, never the emulator's.
    policy: beam_gc::HeapPolicy,
    /// The target process's **own** `max_heap_size` setting — the four-key flag map
    /// `process_info(Pid, max_heap_size)` and the `garbage_collection` item render.
    ///
    /// A process that set one through `erlang:process_flag(max_heap_size, …)` reports the
    /// store's setting ([`Runtime::max_heap_setting`](crate::Runtime::max_heap_setting));
    /// every other process reports the one it was born with — the policy's `max_heap_words`
    /// (so `spawn_opt`'s `{max_heap_size, …}` is visible) plus the emulator's `+hmax*` flags.
    max_heap: crate::config::MaxHeapSetting,
    /// The off-heap (MSO + refc binary) overhead, in bytes.
    off_heap_bytes: usize,
    /// The process's **live owned off-heap refc binaries**, in slot order, each as
    /// `(payload pointer, payload byte length, payload refcount)` — the value behind the
    /// `binary` item ([`RuntimeContext::binary_value`]). Copied out of
    /// [`Process::off_heap_binaries`](beam_process::Process::off_heap_binaries) so the reply
    /// can be built after the process table borrow ends.
    off_heap_binaries: Vec<(usize, usize, usize)>,
}

/// The atom name of a boolean flag value (`true` / `false`).
fn bool_atom(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

// The argument-taking entry points (`process_info_1`/`process_info_2`) are retained as the
// shared, documented implementation of the item vocabulary: `erlang:process_info/1,2` are
// served through the `beam_bifs` `ProcessHook` seam (see `crate::context::process_bridge`),
// which calls the `*_pid` helpers below — the ones this impl block exposes to it.
#[allow(dead_code)]
impl RuntimeContext<'_> {
    /// `erlang:process_info(Pid)`.
    ///
    /// Answers the fixed [`PI_1_KEYS`] set in that order (all 17 keys for a **registered**
    /// process — `{registered_name, Name}` first — and 16, with `registered_name` omitted, for
    /// an **unregistered** one), `undefined` for a pid that is not in the process table and
    /// `badarg` for a non-pid.
    /// An item this runtime cannot answer is **omitted** — the same mechanism the C uses to
    /// drop an unregistered `registered_name` (see the module docs) — so the list is a
    /// genuine, shorter-by-construction answer rather than a fabricated one; the omitted
    /// names are exactly [`UNSUPPORTED_ITEMS`].
    pub(super) fn process_info_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid_term = *args.first().ok_or(BeamError::BadArg)?;
        let pid = ProcessId::from_eterm(pid_term).map_err(|_| BeamError::BadArg)?;
        let Some(snapshot) = self.snapshot(pid) else {
            return self.atom("undefined");
        };
        let mut entries = Vec::with_capacity(PI_1_KEYS.len());
        for name in PI_1_KEYS {
            if *name == "registered_name" {
                match &snapshot.registered_name {
                    // The C's `nil` skip (`erl_bif_info.c:1175`): an unregistered process omits the item.
                    None => continue,
                    Some(_) => {
                        let value = self.registered_name_value(&snapshot)?;
                        let key = self.atom(name)?;
                        entries.push(terms::make_tuple(self, &[key, value])?);
                        continue;
                    }
                }
            }
            let item = Item::Atom((*name).to_string());
            match self.raw_value(&item, &snapshot)? {
                Some(value) => {
                    let key = self.atom(name)?;
                    entries.push(terms::make_tuple(self, &[key, value])?);
                }
                // An item this runtime cannot answer is omitted (honest omission).
                None => continue,
            }
        }
        terms::make_list(self, &entries)
    }

    /// `erlang:process_info(Pid, ItemOrItems)`.
    ///
    /// `ItemOrItems` may be an item **atom**, a **list** of items, or the 2-tuple
    /// `{dictionary, Key}` — exactly the three forms the C accepts
    /// (`erl_bif_info.c:1294-1354`).
    pub(super) fn process_info_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid_term = *args.first().ok_or(BeamError::BadArg)?;
        let option = *args.get(1).ok_or(BeamError::BadArg)?;

        // Validate and parse the option **before** touching the pid: the C's `badarg` for an
        // unknown item wins over the `undefined` answer for a dead pid.
        let parsed = self.parse_option(option)?;

        let pid = ProcessId::from_eterm(pid_term).map_err(|_| BeamError::BadArg)?;
        let Some(snapshot) = self.snapshot(pid) else {
            return self.atom("undefined");
        };

        match parsed {
            ParsedOption::Single(item) => {
                // The C's `always_wrap = !is_atom(opt)` is clear in the single-atom form, so an
                // *unregistered* `registered_name` is the bare `[]` — not `{registered_name, []}`
                // (verified against OTP 29.1.1: `process_info(P, registered_name)` ⇒ `[]`).
                if matches!(&item, Item::Atom(name) if name == "registered_name")
                    && snapshot.registered_name.is_none()
                {
                    return Ok(NIL);
                }
                let value = self.item_value(&item, &snapshot, false)?;
                let key = item.key(self)?;
                terms::make_tuple(self, &[key, value])
            }
            ParsedOption::List(items) => {
                let mut entries = Vec::with_capacity(items.len());
                for item in &items {
                    let value = self.item_value(item, &snapshot, true)?;
                    let key = item.key(self)?;
                    entries.push(terms::make_tuple(self, &[key, value])?);
                }
                terms::make_list(self, &entries)
            }
        }
    }

    /// The value of one item, honoring the `always_wrap` split for `registered_name`.
    ///
    /// `wrapped` is the C's `always_wrap` flag: when it is set (the list/tuple forms),
    /// `registered_name` answers the name atom or `[]`; when it is clear (the single-atom
    /// form), it answers the name atom or the bare `[]`.
    fn item_value(
        &mut self,
        item: &Item,
        snapshot: &Snapshot,
        wrapped: bool,
    ) -> Result<Eterm> {
        if let Item::Atom(name) = item {
            if name == "registered_name" {
                return self.registered_name_value(snapshot);
            }
        }
        let _ = wrapped;
        match self.raw_value(item, snapshot)? {
            Some(value) => Ok(value),
            None => Err(self.unsupported_item(item)),
        }
    }

    /// The `registered_name` value: the **bare** name atom, or the empty list `[]`.
    ///
    /// The C returns this unwrapped whenever `always_wrap` is clear and the item is not
    /// skipped; in the list form it is wrapped by the caller, so the bare value is what both
    /// paths want here.
    fn registered_name_value(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        match &snapshot.registered_name {
            Some(name) => {
                let name = name.clone();
                self.atom(&name)
            }
            None => Ok(NIL),
        }
    }

    /// The typed capability gap for a valid item this runtime cannot answer.
    ///
    /// The reason names the item *and* the exact missing accessor/hook behind it (from
    /// [`UNSUPPORTED_ITEM_REASONS`]) so a gap is never a vague placeholder. `catchlevel` and
    /// the four execution-position items share the one *conditional* reason: they are served
    /// from the process's engine — the runner state's view, or the executing process's own
    /// live machine through the in-machine BIF door — and this runtime holds neither for a
    /// fun-bodied process with no resolved target, an unscheduled pid, or a direct/no-machine
    /// caller.
    fn unsupported_item(&self, item: &Item) -> BeamError {
        match item {
            Item::Atom(name)
                if POSITION_ITEMS.contains(&name.as_str()) || name.as_str() == "catchlevel" =>
            {
                crate::error::unsupported(&format!(
                    "process_info: {name} (this runtime holds no engine for that process: it is \
                     a fun-bodied process with no resolved target, an unscheduled pid, or a \
                     direct/no-machine caller)"
                ))
            }
            Item::Atom(name) => {
                let why = UNSUPPORTED_ITEM_REASONS
                    .iter()
                    .find(|(item, _)| item == name)
                    .map(|(_, why)| *why)
                    .unwrap_or("no backing accessor is reachable from this floor");
                crate::error::unsupported(&format!("process_info: {name} ({why})"))
            }
            Item::DictionaryKey(_) => crate::error::unsupported("process_info: {dictionary,_}"),
        }
    }

    /// The value of one **execution-position** item — where the process is executing.
    ///
    /// The terms come from [`Machine`](beam_engine::Machine)'s read-only accessors (`beam_engine`), the only
    /// builders of those shapes. Three sources answer, in this order: the runtime's **root
    /// execution frame** for the runtime's own `-eval`/`-s`/`-run` process
    /// ([`Self::root_frame_value`]); the **in-machine door**'s published slot for the
    /// executing process's **own** query ([`crate::context::position`] — the scheduler takes
    /// that machine *out* of the runner state for the whole slice, so this is the only source
    /// that can name it); then the runner state's view, where the module whose `M`/`F`/`A`
    /// atoms a position resolves in is the process's **entry** module and the engines of the
    /// current scheduler pass (else the runtime's own) are what
    /// [`RunnerState::engine_at`](crate::runner::RunnerState::engine_at) is read through.
    ///
    /// `Ok(None)` — the caller's honest "cannot answer", turned into an omission by
    /// `process_info/1` and the typed capability gap by `process_info/2` — when this runtime
    /// holds no execution position for `pid`: it is a **fun-bodied** process whose local
    /// target is unresolved (its entry is a fun value, so no module could name `M`/`F`
    /// without inventing one), a pid this runtime never scheduled, or a **direct/no-machine**
    /// caller (one that reached `call_bif` without a `Machine`, so no in-machine door is
    /// open). The caller's own position **is** answered when it is running compiled code —
    /// the engine hands the live `Machine` to the in-machine door above. A position the
    /// engine cannot resolve inside a module it *does* have (a `func` index out of range) is
    /// its own typed error, propagated as-is — never replaced by a plausible-looking term.
    fn position_value(&mut self, name: &str, pid: ProcessId) -> Result<Option<Eterm>> {
        // The runtime's **own** process, when a root action drives it (`-eval`/`-s`/`-run`):
        // the oracle answers those items from that action's own driver frame, which this
        // crate models in the root execution frame (see [`Self::root_frame_value`]).
        if let Some(action) = self.runtime.root_action_of(pid) {
            if let Some(value) = self.root_frame_value(name, action)? {
                return Ok(Some(value));
            }
        }
        // The **in-machine door** (`crate::context::position`): the executing process's own
        // machine is not in the runner state this reads below (the scheduler takes it out
        // for the whole slice), but the in-machine BIF door publishes its five machine-
        // sourced answers here for the one nested `process_info` call that needs them. It
        // comes after the root frame so a `-eval`/`-s`/`-run` root process keeps that
        // frame's answers, and before the runner state because no other source can name the
        // caller's own position at all.
        if let Some(value) = self.in_machine_value(name, pid) {
            return Ok(Some(value));
        }
        let Some(engines) = self.engines() else {
            return Ok(None);
        };
        // `position_engine_at` (not `engine_at`) so a **fun-bodied** process whose local
        // target has been resolved answers too: its entry lambda's module names its
        // position exactly as an export entry's module names that one (see the method's
        // docs for the oracle evidence).
        let Some((module, machine, applied)) = engines.position_engine_at(pid) else {
            return Ok(None);
        };
        match name {
            // A **fun-bodied** process's `$initial_call` is `erlang:apply/2` — ERTS
            // spawned it by *applying* the fun (oracle: `process_info(P, initial_call)`
            // of `spawn(fun() -> … end)` ⇒ `{initial_call,{erlang,apply,2}}`), so the
            // entry lambda's own MFA would be a wrong answer.
            "initial_call" if applied => self.apply_initial_call().map(Some),
            "current_function" => machine.current_function(module, self),
            "initial_call" => machine.initial_call(module, self),
            "current_location" => machine.current_location(module, self),
            "current_stacktrace" => machine.current_stacktrace(module, self).map(Some),
            // Unreachable: only the four names above are routed here.
            _ => Ok(None),
        }
    }

    /// The value of one execution-position item sourced from the runtime's **root execution
    /// frame** — the `-eval`/`-s`/`-run` root action driving the caller itself
    /// ([`crate::runtime::executing`]).
    ///
    /// The frame carries *how* the runtime's own process was started, and two of the oracle's
    /// answers follow from that alone (all measured on the system `erl`, OTP 29.1.1 /
    /// erts-17.1, through `-eval`/`-s`/`-run`):
    ///
    /// * `initial_call` is `{erlang,apply,2}` for **every** start-instruction-driven process —
    ///   the `-eval` root *and* a module function reached from it. ERTS spawned it by
    ///   *applying* an export, exactly as it does for a fun-bodied `spawn/1` process.
    /// * `catchlevel` is `1` — the root **driver's own** catch frame: the oracle reports `1`
    ///   for a `-eval` root and for `-s`/`-run` alike (the frame is erl_eval's / `init`'s,
    ///   whatever the evaluated expression nests inside it).
    ///
    /// `current_function`/`current_location` name the **innermost** frame, so they are only
    /// sourceable for the `-eval` driver itself ([`crate::runtime::RootAction::Eval`] while no
    /// module frame has been entered, i.e. [`RuntimeContext::executing_module`] is `None`):
    /// the oracle then answers `{erl_eval,do_apply,7}` with
    /// `[{file,"erl_eval.erl"},{line,1044}]` for *any* evaluated expression (verified for
    /// literals, `case`, comprehensions, `catch`/`try`, a fun literal applied, and a nested
    /// `receive`). Inside a real module frame the innermost function lives in the machine,
    /// which the position reader cannot see (see the module docs), so the item falls through
    /// to the honest typed gap.
    ///
    /// `Ok(None)` for every other name — `current_stacktrace` in particular, whose frame list
    /// is fixed by *which* erl_eval function the driver is suspended inside and is therefore
    /// not derivable from this runtime's evaluator — so the caller falls through to the engine
    /// path (and usually to the typed gap).
    fn root_frame_value(
        &mut self,
        name: &str,
        action: crate::runtime::RootAction,
    ) -> Result<Option<Eterm>> {
        // `-eval` with a compiled module frame entered (a module function reached from the
        // evaluated expression): the innermost position is that function's, which is not
        // sourceable here — fall through to the gap rather than answer the driver's frame.
        let evaluator_drives = matches!(action, crate::runtime::RootAction::Eval)
            && self.executing_module().is_none();
        match name {
            "initial_call" => self.apply_initial_call().map(Some),
            "current_function" if evaluator_drives => {
                let mfa = self.eval_driver_mfa()?;
                terms::make_tuple(self, &mfa).map(Some)
            }
            "current_location" if evaluator_drives => self.eval_driver_location().map(Some),
            "catchlevel" => Ok(Some(make_small(1))),
            _ => Ok(None),
        }
    }

    /// `{erlang, apply, 2}` — the `$initial_call` the oracle reports for a process ERTS
    /// started by **applying** something (a fun-valued `spawn`, or a start instruction)
    /// rather than by entering an export. The atoms are interned by name because no loaded
    /// module owns this pair.
    fn apply_initial_call(&mut self) -> Result<Eterm> {
        let module = self.atom("erlang")?;
        let function = self.atom("apply")?;
        terms::make_tuple(self, &[module, function, make_small(2)])
    }

    /// `{erl_eval, do_apply, 7}` — the `-eval` evaluator's own **driver frame**, the MFA the
    /// oracle reports as `current_function` while it evaluates an expression.
    fn eval_driver_mfa(&mut self) -> Result<[Eterm; 3]> {
        Ok([
            self.atom("erl_eval")?,
            self.atom("do_apply")?,
            make_small(7),
        ])
    }

    /// `{erl_eval, do_apply, 7, [{file, "erl_eval.erl"}, {line, 1044}]}` — the `-eval`
    /// evaluator's driver frame as `current_location`.
    ///
    /// `erl_eval.erl`/line `1044` are the oracle's own constants for that frame (the
    /// `erl_eval:do_apply/7` clause the driver is suspended in), not a measured position of
    /// this runtime's evaluator — the file is a plain char list, exactly as a stacktrace item
    /// renders it.
    fn eval_driver_location(&mut self) -> Result<Eterm> {
        let mfa = self.eval_driver_mfa()?;
        let file_tag = self.atom("file")?;
        let file = self.charlist("erl_eval.erl")?;
        let line_tag = self.atom("line")?;
        let file_pair = terms::make_tuple(self, &[file_tag, file])?;
        let line_pair = terms::make_tuple(self, &[line_tag, make_small(1044)])?;
        let location = terms::make_list(self, &[file_pair, line_pair])?;
        terms::make_tuple(self, &[mfa[0], mfa[1], mfa[2], location])
    }

    /// The `catchlevel` item: the process's live exception **catch-frame depth**, as an
    /// integer.
    ///
    /// The C answers `make_small(catchlevel(rp))` (`erl_bif_info.c:2169-2171`) where
    /// `catchlevel(p) = p->catches` (`beam_common.c:2347-2350`) — the number of active
    /// `catch`/`try` frames, incremented by the `catch` instruction and decremented by
    /// `catch_end`/`try_end`. `beam_engine` maintains exactly that stack
    /// ([`Machine::catch_frame_depth`](beam_engine::Machine::catch_frame_depth)), pushed by
    /// `Instr::Try`/`Instr::Catch` and popped by `try_case`/`try_end`/`catch_end`, so this is
    /// the same quantity.
    ///
    /// Conditionality matches the execution-position items: `Ok(None)` — the honest "cannot
    /// answer", an omission in `process_info/1` and the typed capability gap in
    /// `process_info/2` — when this runtime holds no engine for `pid` (a fun-bodied process
    /// whose local target is unresolved, an unscheduled pid, or a direct/no-machine caller).
    /// The **executing** process's own depth is read from its live machine through the
    /// in-machine BIF door. A missing engine is never a fabricated `0`.
    ///
    /// Oracle (OTP 29.1.1 / erts-17.1): `0` for a process parked outside any `try`; `1` for
    /// one parked inside a single `try`; the caller itself reports its own live depth (a
    /// `process_info(self(), catchlevel)` evaluated inside one `try` answers `1`).
    fn catchlevel_value(&mut self, pid: ProcessId) -> Result<Option<Eterm>> {
        // The root execution frame first: the oracle reports the root driver's own catch
        // frame (`1`) for a `-eval`/`-s`/`-run` process (see [`Self::root_frame_value`]).
        if let Some(action) = self.runtime.root_action_of(pid) {
            if let Some(value) = self.root_frame_value("catchlevel", action)? {
                return Ok(Some(value));
            }
        }
        // The in-machine door's published depth (see [`Self::position_value`]).
        if let Some(value) = self.in_machine_value("catchlevel", pid) {
            return Ok(Some(value));
        }
        let Some(engines) = self.engines() else {
            return Ok(None);
        };
        let Some((_module, machine, _applied)) = engines.position_engine_at(pid) else {
            return Ok(None);
        };
        Ok(Some(make_small(machine.catch_frame_depth() as i64)))
    }

    /// An item's value from this runtime's real state, or `None` when the item is a valid
    /// oracle name this runtime cannot answer.
    fn raw_value(&mut self, item: &Item, snapshot: &Snapshot) -> Result<Option<Eterm>> {
        let name = match item {
            Item::DictionaryKey(key) => {
                let value = snapshot
                    .dictionary
                    .iter()
                    .find(|(stored, _)| stored == key)
                    .map(|(_, value)| *value);
                return match value {
                    Some(value) => Ok(Some(value)),
                    None => Ok(Some(self.atom("undefined")?)),
                };
            }
            Item::Atom(name) => name.as_str(),
        };
        let value = match name {
            // Handled by the callers (`item_value`/`process_info_1`).
            "registered_name" => return Ok(None),
            // The execution-position items: the engine's read-only view of *where* the
            // process is (see [`Self::position_value`]).
            "current_function" | "initial_call" | "current_location" | "current_stacktrace" => {
                return self.position_value(name, snapshot.pid)
            }
            // The catch level (the C's `catchlevel(rp)` = `rp->catches`,
            // `beam_common.c:2347-2350`, `erl_bif_info.c:2169-2171`): the depth of the
            // process's live exception catch-frame stack. Read from the engine like the
            // position items — see [`Self::catchlevel_value`].
            "catchlevel" => return self.catchlevel_value(snapshot.pid),
            "status" => self.atom(snapshot.status)?,
            "message_queue_len" => make_small(snapshot.message_queue_len as i64),
            "messages" => terms::make_list(self, &snapshot.messages)?,
            "links" => terms::make_list(self, &snapshot.links)?,
            "monitors" => self.monitors_value(snapshot)?,
            "monitored_by" => terms::make_list(self, &snapshot.monitored_by)?,
            "dictionary" => self.dictionary_value(snapshot)?,
            "trap_exit" => {
                let atom = bool_atom(snapshot.trap_exit);
                self.atom(atom)?
            }
            // The C's per-process `F_ASYNC_DIST` flag (`erl_bif_info.c:986,2981`), read out of
            // the runtime's own per-pid store — the very store
            // `process_flag(async_dist, …)` ([`crate::context::process_bridge`]) writes, so the
            // read and the write can never disagree. A pid that never set the flag reports the
            // fresh-process default `false`, the oracle's answer.
            "async_dist" => {
                let atom = bool_atom(self.runtime.is_async_dist(snapshot.pid));
                self.atom(atom)?
            }
            "error_handler" => self.atom("error_handler")?,
            "priority" => self.atom(snapshot.priority)?,
            "group_leader" => snapshot.group_leader,
            "heap_size" => make_small(snapshot.heap_size as i64),
            "total_heap_size" => make_small(snapshot.total_heap_size as i64),
            "stack_size" => make_small(snapshot.stack_size as i64),
            "reductions" => make_small(snapshot.reductions as i64),
            "garbage_collection" => self.garbage_collection(snapshot)?,
            "min_heap_size" => make_small(snapshot.policy.min_heap_words as i64),
            "min_bin_vheap_size" => make_small(snapshot.policy.min_bin_vheap_words as i64),
            "fullsweep_after" => make_small(i64::from(snapshot.policy.fullsweep_after)),
            "max_heap_size" => {
                let setting = snapshot.max_heap;
                self.max_heap_size_map(setting)?
            }
            // This runtime's real, current queue placement: [`beam_message::Queue`] is
            // heap-resident, so `on_heap` is the true value (an explicit
            // `process_flag(message_queue_data, off_heap)` is accepted-and-ignored — see the
            // divergence note in the module docs).
            "message_queue_data" => self.atom("on_heap")?,
            // The process's **live** tracee-flag bitmask (the C's
            // `ERTS_IS_P_TRACED_FL(rp, TRACEE_FLAGS)`, `erl_bif_info.c:2133`) — read out of the
            // trace state `erlang:trace/3` maintains, translated to ERTS's bit layout by
            // [`trace_bitmask`]. Untraced ⇒ `0`, the oracle's answer.
            "trace" => {
                let flags = self
                    .runtime
                    .trace()
                    .get(snapshot.pid)
                    .map(|entry| entry.flags)
                    .unwrap_or_default();
                make_small(trace_bitmask(flags))
            }
            // The sequential-trace token (contract A.8): the C copies the process's token
            // tuple verbatim, which is the bare `[]` while that process's tracing is inactive
            // (`erl_bif_info.c:2162-2166`). Read out of [`Runtime`](crate::Runtime)'s
            // sequential-tracing state through [`crate::seq_trace`].
            "sequential_trace_token" => match self.runtime.seq_trace().token(snapshot.pid) {
                Some(token) => self.sequential_trace_token(token)?,
                None => NIL,
            },
            // An unchanging M1 default: no `suspend_process`.
            "suspending" => NIL,
            // The oracle's own answer for a process with no saved-calls buffer
            // (`erl_bif_info.c:2184-2188` returns `am_false`), which is every process this
            // runtime runs: it never saves calls, so there is no `call`-flag call list to
            // report. Not a placeholder — the true value.
            "last_calls" => self.atom("false")?,
            // The process's off-heap refc binaries as the oracle's `[{Ptr, Size, Refc}, …]`
            // roster (the empty list when it owns none) — see [`Self::binary_value`].
            "binary" => self.binary_value(snapshot)?,
            "memory" => make_small(
                ((snapshot.total_heap_size + snapshot.stack_size) * 8 + snapshot.off_heap_bytes)
                    as i64,
            ),
            // A valid oracle item this runtime cannot answer → the caller raises.
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    /// The `monitors` value: `[{process | port, Pid}]`.
    fn monitors_value(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        let mut entries = Vec::with_capacity(snapshot.monitors.len());
        for (is_process, target) in &snapshot.monitors {
            let kind = self.atom(if *is_process { "process" } else { "port" })?;
            entries.push(terms::make_tuple(self, &[kind, *target])?);
        }
        terms::make_list(self, &entries)
    }

    /// The `dictionary` value: the stored `{Key, Value}` pairs, insertion order.
    fn dictionary_value(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        let mut entries = Vec::with_capacity(snapshot.dictionary.len());
        for (key, value) in &snapshot.dictionary {
            entries.push(terms::make_tuple(self, &[*key, *value])?);
        }
        terms::make_list(self, &entries)
    }

    /// The `garbage_collection` value: the oracle's five `{Key, Value}` pairs, in
    /// [`beam_gc::GARBAGE_COLLECTION_KEYS`] order.
    fn garbage_collection(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        let policy = &snapshot.policy;
        let info = HeapInfo::new(
            policy,
            snapshot.minor_gcs,
            snapshot.heap_size,
            snapshot.total_heap_size,
        );
        let max_heap = snapshot.max_heap;
        let mut entries = Vec::with_capacity(5);
        for (key, value) in info.items() {
            let value = match key {
                // The one item whose value is a container, and the one whose figure is the
                // process's own setting rather than the policy's (`process_flag` can replace
                // it): the four-key flag map.
                "max_heap_size" => self.max_heap_size_map(max_heap)?,
                _ => make_small(value as i64),
            };
            let key = self.atom(key)?;
            entries.push(terms::make_tuple(self, &[key, value])?);
        }
        terms::make_list(self, &entries)
    }

    /// The `max_heap_size` process-flag map: `#{size => N, kill => F, error_logger => F,
    /// include_shared_binaries => F}` — the setting the process reports (see
    /// [`Snapshot::max_heap`]), rendered in the C's own key order
    /// (`erts_max_heap_size_map`, `erl_gc.c:3807-3822`, verified against OTP 29.1.1 /
    /// erts-17.1: `error_logger`, `include_shared_binaries`, `kill`, `size`).
    ///
    /// `size` is the raw word bound (`0` = unlimited), never a growth-table rounding.
    /// `pub(crate)`: the `process_flag/2` bridge builds the **previous** value it returns
    /// through this same builder ([`crate::context::process_bridge`]), so the two can never
    /// render the map differently.
    pub(crate) fn max_heap_size_map(&mut self, setting: MaxHeapSetting) -> Result<Eterm> {
        let flags = setting.flags;
        let booleans = [
            ("error_logger", flags.error_logger),
            ("include_shared_binaries", flags.include_shared_binaries),
            ("kill", flags.kill),
        ];
        let mut pairs = Vec::with_capacity(booleans.len() + 1);
        for (key, flag) in booleans {
            let value = self.atom(bool_atom(flag))?;
            pairs.push((self.atom(key)?, value));
        }
        pairs.push((self.atom("size")?, make_small(setting.size as i64)));
        // An owned handle to the shared table keeps the caller's `&mut self` free for the
        // map builder (`terms::make_map` needs both the context and the table).
        let atoms = self.runtime.code_server.atoms_handle();
        terms::make_map(self, &pairs, &atoms)
    }

    /// `pid`'s observables, or `None` when the process table has no such process.
    ///
    /// The table read is [`RuntimeContext::process_table`] — the *pass* table when this
    /// context is a scheduler slice (the runtime's own table is an empty placeholder then),
    /// so a process probing **another** scheduled process sees it.
    fn snapshot(&self, pid: ProcessId) -> Option<Snapshot> {
        let table = self.process_table();
        let process = table.get(pid)?;
        // The oracle's `process_info(Pid, _)` answers the bare atom `undefined` — for **any**
        // item — once the pid names a dead process; it does not keep a terminated PCB around
        // long enough to answer `{status, exiting}`: by the time a neighbour's crash report
        // (or `monitor`/link sweep) observes the pid, the PCB is gone and every item is
        // `undefined`. This runtime's table still holds the entry for the whole
        // `Exiting`/`Terminated` window (the crash reporter runs *before* the reaper), so
        // treat that window as absent here too. Without this
        // `process_info(Pid, current_function)` on a just-dead neighbour raised the
        // **uncatchable** `unsupported` capability gap — which `proc_lib:get_process_info/2`'s
        // `try … catch _:_ -> undefined` cannot catch — and killed the crash reporter instead
        // of letting it report the neighbour.
        match process.state() {
            beam_process::ProcessState::Exiting | beam_process::ProcessState::Terminated => {
                return None
            }
            _ => {}
        }
        process.status()?;
        let atoms = self.runtime.code_server.atoms();
        let registered_name = table
            .registered()
            .find(|(_, registered)| *registered == pid)
            .and_then(|(name, _)| atoms.name(name).map(str::to_string));
        // The oracle's `monitors` item carries **both** monitor families, each entry as
        // `{process, Pid}` or `{port, Port}` (verified against OTP 29.1.1 / erts-17.1:
        // `monitors => [{port,#Port<0.3>},{process,<0.84.0>}]`). Process monitors live in
        // `Process::monitors`, port monitors in `Process::port_monitors` (populated by
        // `ProcessTable::monitor_port`), so both sets are appended here. The oracle orders
        // the two families by monitor-reference creation; this runtime holds them in two
        // sets and therefore lists the process monitors first. That ordering is unobservable
        // here: `erlang:monitor(port, Port)` is a typed capability gap on this runtime, so
        // the port family is never non-empty at runtime.
        let mut monitors: Vec<(bool, Eterm)> = process
            .monitors()
            .iter()
            .map(|m| (matches!(m.kind, MonitorKind::Process), m.target.to_eterm()))
            .collect();
        monitors.extend(
            process
                .port_monitors()
                .iter()
                .map(|watch| (false, watch.port.to_eterm())),
        );
        Some(Snapshot {
            pid,
            registered_name,
            // This runtime is single-threaded, so exactly one process is executing at a time:
            // the one making this call **is** `running` (the oracle answers `running` for the
            // caller), while every other live process carries its own status —
            // [`Process::status`](beam_process::Process::status), which layers the transient
            // `garbage_collecting` flag over [`ProcessState::status_atom`] (the C's
            // `erts_process_state2status`).
            status: if pid == self.pid() {
                "running"
            } else {
                // A fully `Terminated` table entry has no oracle status at all; the runtime
                // never keeps one (`ProcessTable::take` removes it), so the `exiting`
                // fallback is unreachable in practice and never a fabricated live status.
                process.status().unwrap_or("exiting")
            },
            message_queue_len: process.mailbox().len(),
            messages: process.mailbox().iter().map(|m| m.payload()).collect(),
            links: process.links().iter().map(ProcessId::to_eterm).collect(),
            monitors,
            monitored_by: process
                .monitored_by()
                .iter()
                .map(|m| m.watcher.to_eterm())
                .collect(),
            dictionary: process.dict().entries().collect(),
            trap_exit: process.is_trapping_exit(),
            priority: process.priority().name(),
            group_leader: self.runtime.group_leader_of(pid).to_eterm(),
            heap_size: process.collector().heap_size_words(),
            total_heap_size: process.collector().total_heap_size_words(),
            stack_size: process.registers().stack().len(),
            reductions: process.reductions(),
            minor_gcs: process.collector().minor_gcs(),
            policy: process.collector().policy().clone(),
            // The process's own flag store wins; a process that never called
            // `process_flag(max_heap_size, …)` reports the setting it was born with.
            max_heap: self.runtime.max_heap_setting(pid).unwrap_or_else(|| {
                MaxHeapSetting {
                    size: process.collector().policy().max_heap_words,
                    flags: self.runtime.config().heap_options().max_heap_flags(),
                }
            }),
            off_heap_bytes: process.off_heap().overhead_bytes(),
            off_heap_binaries: process
                .off_heap_binaries()
                .map(|binary| (binary.data_ptr() as usize, binary.len(), binary.refcount()))
                .collect(),
        })
    }

    /// Parses the second argument into the C's three accepted forms.
    fn parse_option(&self, option: Eterm) -> Result<ParsedOption> {
        if beam_term::is_atom(option) {
            return Ok(ParsedOption::Single(self.parse_item(option)?));
        }
        if let Ok(list) = terms::list_elements(option) {
            let mut items = Vec::with_capacity(list.len());
            for element in list {
                items.push(self.parse_item(element)?);
            }
            return Ok(ParsedOption::List(items));
        }
        // Anything else — the `{dictionary, Key}` 2-tuple, an improper list, or a term that
        // is neither atom/list/tuple — goes through `parse_item`, which accepts the
        // `{dictionary, _}` tuple and answers `badarg` for everything else.
        Ok(ParsedOption::Single(self.parse_item(option)?))
    }

    /// Parses one item term (an atom, or the `{dictionary, Key}` 2-tuple).
    fn parse_item(&self, term: Eterm) -> Result<Item> {
        if let Some(name) = self.atom_name(term) {
            if IMPLEMENTED_ITEMS.contains(&name.as_str())
                || UNSUPPORTED_ITEMS.contains(&name.as_str())
            {
                return Ok(Item::Atom(name));
            }
            return Err(BeamError::BadArg);
        }
        if let Ok(2) = terms::tuple_arity(term) {
            let tag = terms::tuple_element(term, 0)?;
            if self.atom_name(tag).as_deref() == Some("dictionary") {
                return Ok(Item::DictionaryKey(terms::tuple_element(term, 1)?));
            }
        }
        Err(BeamError::BadArg)
    }
    /// Whether `name` is an item this runtime reports at all — the `valid` half of the
    /// oracle's `pi_arg2ix` split (`IMPLEMENTED_ITEMS` ∪ `UNSUPPORTED_ITEMS`).
    ///
    /// The `ProcessHook` `process_info/2` bridge validates the item through this **before**
    /// it inspects the process, exactly as the C does, so an unknown item is `badarg` even
    /// for a dead pid.
    pub(crate) fn is_known_process_info_item(&self, name: &str) -> bool {
        IMPLEMENTED_ITEMS.contains(&name) || UNSUPPORTED_ITEMS.contains(&name)
    }

    /// The typed capability gap for a valid item this runtime cannot answer.
    pub(crate) fn unsupported_process_info_item(&self, name: &str) -> BeamError {
        self.unsupported_item(&Item::Atom(name.to_string()))
    }

    /// The item **names** `erlang:process_info/1` reports for `pid`, in [`PI_1_KEYS`] order,
    /// or `None` when `pid` names no live process (the oracle's `undefined`).
    ///
    /// This is the same walk [`Self::process_info_1`] performs: an unregistered
    /// `registered_name` is skipped (the C's `nil` skip) and an execution-position item this
    /// runtime holds no position for is omitted, so the hook can never be handed an item it
    /// could not then resolve.
    pub(crate) fn process_info_keys_pid(&mut self, pid: ProcessId) -> Result<Option<Vec<Atom>>> {
        let Some(snapshot) = self.snapshot(pid) else {
            return Ok(None);
        };
        let mut keys = Vec::with_capacity(PI_1_KEYS.len());
        for name in PI_1_KEYS {
            if *name == "registered_name" && snapshot.registered_name.is_none() {
                continue;
            }
            if POSITION_ITEMS.contains(name) && self.position_value(name, pid)?.is_none() {
                continue;
            }
            let atom = self
                .runtime
                .code_server
                .atoms()
                .lookup(name)
                .ok_or(BeamError::TermNotFound)?;
            keys.push(atom);
        }
        Ok(Some(keys))
    }

    /// The value of one known `erlang:process_info/2` item for `pid`, or `None` when the
    /// item is one this runtime cannot answer (a typed capability gap — the caller raises).
    ///
    /// `name` must already have passed [`Self::is_known_process_info_item`], and `pid` must
    /// be live.
    pub(crate) fn process_info_item_value(
        &mut self,
        pid: ProcessId,
        name: &str,
    ) -> Result<Option<Eterm>> {
        let Some(snapshot) = self.snapshot(pid) else {
            return Ok(None);
        };
        if name == "registered_name" {
            return self.registered_name_value(&snapshot).map(Some);
        }
        self.raw_value(&Item::Atom(name.to_string()), &snapshot)
    }
}

/// The parsed second argument of `erlang:process_info/2`.
enum ParsedOption {
    /// A single item (an atom, or `{dictionary, Key}`).
    Single(Item),
    /// A list of items, in the requested order (duplicates kept).
    List(Vec<Item>),
}

#[cfg(test)]
mod binary_e2e;
#[cfg(test)]
mod tests;
