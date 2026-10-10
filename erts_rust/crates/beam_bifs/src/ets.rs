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

//! The `ets:*` bridge: a [`TableRegistry`] mapping opaque table handles to
//! `beam_ets::Table`s, plus the `ets:*` BIF entry points built on top of it.
//!
//! `beam_ets` owns the table semantics; this module owns the M1 runtime side — a
//! map from a handle to a live table, the `ets:new/2` option parser (the child
//! module [`resolve`]) and the registered BIFs. The *whole-table* BIFs
//! (`new`/`delete/1`/`delete_all_objects`/`info`/`tab2list`), the *mutation* BIFs
//! (`insert`/`insert_new`/`delete/2`/`delete_object`/`take`/`update_counter/3,4`/
//! `update_element/3`), the *lookup* BIFs (`lookup`/`lookup_element/3,4`/`member`)
//! and the *navigation* BIFs (`first`/`last`/`next`/`prev`) all delegate to the
//! corresponding [`Table`] primitive; the bridging work is the handle, the
//! `badarg` contract and the term movement across the table boundary.
//!
//! # The M1 table handle
//!
//! Real ERTS identifies a table by a `tid()`. Milestone-1 has no `make_ref` in
//! `beam_term`, so [`crate::access::make_ref_thing`] builds the frozen 4-word ref
//! thing `[make_header(3, REF_SUBTAG), 0, 0, id]` and its numeric `id` (read back
//! by [`crate::access::ref_thing_id`]) is the registry key (mirroring
//! `beam_process::MonitorRef::to_eterm`). When `beam_term` gains a proper
//! `make_ref`, only the handle constructor changes (see the crate `CONTEXT.md`,
//! Design Decisions / Known Issues).
//!
//! # Why the BIFs move the table out and back
//!
//! A BIF has one `&mut BifContext`, and both `ctx.atoms()` (`&AtomTable`) and
//! `ctx.tables()` (`&mut TableRegistry`) borrow it — so `table.insert(obj,
//! ctx.atoms())` cannot be written while the registry borrow is live. The BIFs
//! therefore *take* the table out of the registry (releasing the borrow), operate
//! on the owned table with `ctx.atoms()`/`ctx.heap()`, then *restore* it — even on
//! the error path, so a failed BIF never loses the table. This is safe for the
//! terms a table hands back: a [`beam_ets::Table`]'s term storage is a
//! **never-relocating bump arena** of stable block pointers, so moving the `Table`
//! handle does not move any term, and the aliased object/key is copied onto the
//! caller's heap ([`beam_message::copy_struct_simple`]) before it is returned.
//!
//! # `ets:info/1,2` and the info-field set
//!
//! `ets:info/1` renders the oracle's **15** fields in the oracle's order — `id`,
//! `decentralized_counters`, `read_concurrency`, `write_concurrency`, `compressed`,
//! `memory`, `owner`, `heir`, `name`, `size`, `node`, `named_table`, `type`,
//! `keypos`, `protection` — and `ets:info/2` answers a single `Item`. The mapping
//! is written here from the public [`TableInfo`] fields rather than through
//! [`TableInfo::entries`], because that helper needs a `&mut AtomTable` and a BIF
//! may only borrow the atom table immutably (`ctx.atoms()`), while the field-name
//! and value atoms are interned through [`BifContext::intern_atom`]. This
//! reproduces [`TableInfo::entries`] field-for-field (see [`info_field`]), with one
//! deliberate difference: `id` is the table's **tid reference** — the very handle
//! ref thing [`crate::access::make_ref_thing`] built for this table, so
//! `ets:info(T, id) =:= T` and `is_reference(ets:info(T, id))` both hold, exactly as
//! in the oracle (`TableInfo::id` is the raw numeric id, which is only the
//! registry key here). `ets:info(T, Item)` raises `badarg` for an unknown `Item` of
//! a **live** table — the verified system `erl` behaviour (it does **not** return
//! `undefined`).
//!
//! A *well-formed* table argument that names no live table is the other way round:
//! both arities answer the bare atom `undefined` (verified on erts-17.1) — an unknown
//! atom, an atom naming a table that was *not* created with `named_table`, and a
//! deleted table's handle (`ets:info(T)` / `ets:info(T, name)` after `ets:delete(T)`)
//! are all `undefined`, and that resolution happens **before** the `Item` is looked
//! at, so `ets:info(bogus, 42)` is `undefined` while `ets:info(T, 42)` on a live `T`
//! is `badarg`. A term that is neither an atom nor a reference (`42`, `{T, Node}`,
//! `[]`, a pid) stays `badarg` for both.
//!
//! # The match family (see [`matches`] for the BIFs)
//!
//! The pattern-based half of the `ets` API lives in the child module [`matches`]:
//! `ets:match/1,2,3`, `ets:match_object/1,2,3` and the verb BIFs
//! (`ets:select_count/2`, `ets:select_delete/2`, `ets:select_replace/2`,
//! `ets:match_delete/2`), sharing this module's continuation registry, its
//! handle/`badarg` contract and its chunk renderer.
//!
//! # The compiled match-spec family (see [`specs`] for the BIFs)
//!
//! `ets:match_spec_compile/1` and `ets:match_spec_run/2` live in the child module
//! [`specs`], which owns the opaque compiled-spec handle and the
//! [`TableRegistry::insert_spec`]/[`TableRegistry::spec_raw`] registry entry that
//! backs it.
//!
//! # The table argument, the owner and access gating (see [`resolve`])
//!
//! Every table-taking BIF resolves its first argument through
//! [`resolve::resolve_handle`]: the M1 handle ref thing, or — for a table created
//! with the `named_table` option — its **name atom** (the oracle's
//! `db_get_table_aux` accepts exactly those two shapes; a `{Name, Node}` 2-tuple is
//! **not** a table argument in OTP 29). `ets:new(Name, [named_table])` returns the
//! **name atom**; every other table the handle reference. The two `ets:info/1,2`
//! BIFs resolve through [`resolve::resolve_handle_opt`] instead, which additionally
//! reports `undefined` for a well-formed argument that names no live table (see the
//! `ets:info/1,2` section above).
//!
//! `ets:new/2` records the calling process as the table owner
//! ([`resolve::caller_owner`]) and the installed heir, and each BIF enforces the
//! oracle's access matrix as the *calling* process through [`resolve::Gate`]
//! ([`take_table_gated`]): a read for `lookup`/`member`/`first`/`next`/`tab2list`/the
//! match+select families/`safe_fixtable`, a write for `insert`/`delete`/`take`/
//! `update_counter`/`update_element`/the delete+replace verbs, and no check at all
//! for `ets:info/1,2`. A denial is `badarg` **and leaves the table in the registry**.
//!
//! # The owner→handle roster (the runtime's reclamation seam)
//!
//! An ETS table belongs to the process that created it, so the *runtime* — which
//! owns process death — must reclaim a dead owner's tables. It cannot see this
//! registry's private table map, so [`TableRegistry`] exposes the two public
//! entry points that adoption needs:
//!
//! * [`TableRegistry::owner_of`] — the pid→owner bridge
//!   (`OwnerId::new(pid.to_eterm().raw() as u64)`), the **same** encoding
//!   [`resolve::caller_owner`] records at `ets:new/2` time, so an owner read back
//!   from a table and a pid converted here always agree;
//! * [`TableRegistry::handles_owned_by`] — every handle a given owner holds,
//!   ascending, **including an unnamed table's** (whose handle is otherwise
//!   unreachable from any public term).
//!
//! A host lists a dead pid's handles, then per table calls
//! `beam_ets::Table::owner_died` and acts on the returned `OwnerDeath`: an
//! unnamed/orphan table is dropped through the existing
//! [`TableRegistry::remove`] (+ [`TableRegistry::remove_name`] for a
//! `named_table`), a live heir takes it over, and `{'ETS-TRANSFER', Tid, FromPid,
//! Data}` is delivered **only** when `heir.data` is `Some` (an arity-2
//! `{heir, Pid}` inherits in silence). [`ets:delete/1`] already drops the profile
//! and the name, so an index handle for a deleted table resolves to no table
//! ([`TableRegistry::get_mut`] is `None`) and is skipped.
//!
//! # The child modules and the remaining gaps (see the crate `CONTEXT.md`)
//!
//! The faithful verbs live in the child modules: `ets:safe_fixtable/2`,
//! `ets:whereis/1`, `ets:update_element/4`, `ets:is_compiled_ms/1` and the
//! `ets:rename/2`/`ets:internal_delete_all/2`/`ets:internal_select_delete/2` group
//! in [`extra`], and the adopted `ets:all/0`, `ets:setopts/2`, `ets:give_away/3`,
//! `ets:foldl/3` and `ets:foldr/3` in [`adopt`]. (`ets:foldl/3`/`foldr/3` apply the
//! fun through the `FunCaller` seam, so a non-empty fold is the typed
//! `unsupported_bif` gap until a host installs it.)
//!
//! Four `ets` keys stay honest typed gaps, each for a reason no `BifFn` can
//! satisfy (see [`adopt`] and [`extra`]): `ets:slot/2` (C-interior `db_slot`), and
//! `ets:internal_request_all/0` (a ref + one reply message per scheduler, which
//! needs process messaging), both seam-backed through the `RuntimeHooks` stub in
//! `src/runtime/other.rs`; plus the `ets:setopts/2` `{protection, _}` sub-option
//! (no post-creation access setter in `beam_ets`). `ets:fun2ms/1` and
//! `ets:repair_continuation/2` are pure `stdlib/src/ets.erl` functions — **not**
//! `bif.tab` entries — so the registry can never dispatch them and they stay
//! unregistered.
//!
//! # The `*_lookup` family (see [`nav`] for the BIFs)
//!
//! `ets:first_lookup/1`, `ets:last_lookup/1`, `ets:next_lookup/2` and
//! `ets:prev_lookup/2` — a navigation step paired with the objects of the key it
//! landed on — live in the child module [`nav`]. For a **hash** kind
//! (`set`/`bag`/`duplicate_bag`) the oracle aliases `last_lookup` to `first` and
//! `prev_lookup` to `next` (a hash interior has no backward walk), while an
//! `ordered_set` walks its real order in both directions.
//!
//! # The reverse select family (see [`reverse`] for the BIFs)
//!
//! `ets:select_reverse/1,2,3` lives in the child module [`reverse`] and is a thin
//! wrapper over `beam_ets`'s native reverse family
//! (`Table::select_reverse`/`select_reverse_limit`/`select_reverse_continue`): for
//! an `ordered_set` results come in **descending** key order, for the hash kinds
//! the oracle leaves the order unspecified and the native form yields the reverse
//! of `select`'s order. Reverse continuations live in the *shared*
//! [`TableRegistry::continuations`] map alongside the forward ones — a
//! [`Continuation`] records the **direction** it was minted with, and
//! `ets:select/1` / `ets:select_reverse/1` share one dispatcher
//! ([`dispatch_continuation`]) so resuming follows the *stored* direction, as the
//! oracle does.

use std::collections::HashMap;

use beam_alloc::Heap;
use beam_common::Result;
use beam_ets::{compile, key_of, MatchContinuation, OwnerId, Table, TableId, TableInfo};
use beam_process::ProcessId;
use beam_term::{atom_val, make_atom, signed_value, Atom, AtomTable, Eterm};

use crate::access::{
    is_atom, is_list, is_nil, is_small, is_tuple, make_ref_thing, ref_thing_id, tuple_element,
    tuple_size,
};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// The `ets:match`/`match_object` families and the count/delete BIFs.
///
/// They share this module's continuation registry, handle contract and chunk
/// renderer, so they are a child of `ets` (declared here, not in `src/lib.rs`).
mod matches;
/// The compiled match-spec family (`ets:match_spec_compile/1`,
/// `ets:match_spec_run/2`): the opaque compiled-spec handle and its registry.
///
/// It shares this module's handle contract and [`TableRegistry`]'s compiled-spec
/// map, so it is a child of `ets` (declared here, not in `src/lib.rs`).
mod specs;
/// The remaining faithful `ets:*` verbs (`ets:safe_fixtable/2`, `ets:whereis/1`,
/// `ets:update_element/4`, `ets:is_compiled_ms/1`).
///
/// They share this module's handle contract, [`TableRegistry`] and helpers
/// (`take_table`/`restore_table`/`parse_updates`), so they are a child of `ets`
/// (declared here, not in `src/lib.rs`). The verbs that stay unregistered, and
/// why, are documented there.
mod extra;
/// The adopted `ets:*` verbs (`ets:all/0`, `ets:setopts/2`, `ets:give_away/3`,
/// `ets:foldl/3`, `ets:foldr/3`) plus the honest typed gaps for the verbs that
/// cannot be served over the frozen `beam_ets::Table` API.
///
/// They share this module's handle contract, [`TableRegistry`] and helpers
/// (`take_table_gated`/`restore_table`/`copy_out`) and the shared resolver
/// [`resolve`], so they are a child of `ets` (declared here, not in `src/lib.rs`).
/// Every gap is documented there with the exact `beam_ets` API it needs.
mod adopt;
/// The key/navigation `*_lookup` family (`ets:first_lookup/1`, `last_lookup/1`,
/// `next_lookup/2`, `prev_lookup/2`).
///
/// They pair a navigation step with the objects of the key it landed on, so they
/// share this module's handle contract, its helpers (`take_table_gated`/
/// `restore_table`/`copy_out`/`copy_out_list`/`key_or_end`) and the shared
/// resolver [`resolve`] — a child of `ets` (declared here, not in `src/lib.rs`).
mod nav;
/// The reverse select family (`ets:select_reverse/1,2,3`).
///
/// It is a thin wrapper over `beam_ets`'s native reverse primitives
/// (`Table::select_reverse`/`select_reverse_limit`); it shares this module's handle
/// contract, its helpers (`take_table_gated`/`restore_table`/`copy_out_list`/
/// `finish_chunk`), the shared resolver [`resolve`] and the shared continuation
/// registry — a child of `ets` (declared here, not in `src/lib.rs`).
mod reverse;
/// The shared table-argument resolver, the process/owner bridge, the
/// creation-time [`resolve::TableProfile`] and the `ets:new/2` option parser.
///
/// Every table-taking `ets:*` BIF resolves its first argument through
/// [`resolve::resolve_handle`] and gates the operation as the calling process
/// through [`resolve::Gate`], so it is a child of `ets` (declared here, not in
/// `src/lib.rs`).
mod resolve;
/// The navigation terminator (`ets:first/1` … `ets:prev/2` at the end of the
/// iteration order). The atom's *name* is `$end_of_table` (the printname of
/// `atom.names`' `EOT='$end_of_table'`).
pub(super) const END_OF_TABLE: &str = "$end_of_table";

/// The `ets:info/1` field names, in the oracle's order (the reverse of the C
/// `fields[]` array in `erl_db.c`, reproduced by [`TableInfo::entries`]).
const INFO_ITEMS: &[&str] = &[
    "id",
    "decentralized_counters",
    "read_concurrency",
    "write_concurrency",
    "compressed",
    "memory",
    "owner",
    "heir",
    "name",
    "size",
    "node",
    "named_table",
    "type",
    "keypos",
    "protection",
];

/// The base of the continuation-id namespace.
///
/// [`TableId::next`] counts up from `1`, so a continuation id is allocated from a
/// high, disjoint base (`1 << 40`, well inside [`crate::access::make_ref_thing`]'s
/// 60-bit id range) and can never collide with a table handle — a continuation ref
/// thing therefore never compares `=:=` to a table handle.
const CONTINUATION_ID_BASE: u64 = 1 << 40;

/// The base of the compiled-match-spec id namespace.
///
/// A compiled spec (see [`specs`]) is another on-heap ref thing whose id is a
/// registry key, so it needs its own namespace: this base is disjoint from the
/// table-handle ids ([`TableId::next`] counts up from `1`) and from
/// [`CONTINUATION_ID_BASE`] (`1 << 40` — a run would need ~2^40 live continuations
/// before its counter could reach this base), so a compiled-spec ref never compares
/// `=:=` to a table handle or a continuation, and vice versa.
const SPEC_ID_BASE: u64 = 1 << 48;

/// A live `ets:select/3` continuation: the traversal state plus the handle of the
/// table it belongs to, so `ets:select/1` can find the table again.
struct Continuation {
    /// The registry handle of the table the traversal runs against.
    table: u64,
    /// The opaque traversal state [`beam_ets::Table::select_continue`] resumes.
    state: MatchContinuation,
    /// The **direction** the traversal was minted with: `true` for an
    /// `ets:select_reverse/3` chunk, `false` for `ets:select/3`/`ets:match/3`/
    /// `ets:match_object/3`. `ets:select/1` and `ets:select_reverse/1` are one BIF
    /// following the *stored* direction (the oracle), so the dispatcher resumes a
    /// reverse continuation through `Table::select_reverse_continue` and a forward
    /// one through `Table::select_continue`, and a chunk minted while resuming
    /// keeps the direction.
    reverse: bool,
}

/// The runtime's ETS tables, keyed by the numeric id of the on-heap ref thing
/// built by [`crate::access::make_ref_thing`].
#[derive(Default)]
pub struct TableRegistry {
    tables: HashMap<u64, Table>,
    /// The `named_table` name registry — the oracle's *meta name table*: the name
    /// atom of every table created with the `named_table` option, mapped to its
    /// handle.
    ///
    /// [`new_2`] populates it (rejecting a name that is already taken, as the
    /// oracle's duplicate `ets:new(Name, [named_table])` is `badarg`), [`delete_1`]
    /// clears the entry of a deleted named table, and `ets:whereis/1` plus the
    /// name-atom table argument the verbs accept (see [`extra`]) read it. A table
    /// created *without* `named_table` only records a `name` field
    /// ([`Table::name`]) and never appears here — exactly as in the oracle, where
    /// `ets:whereis(Name)` is `undefined` for such a table.
    names: HashMap<Atom, u64>,
    /// The live `ets:select/3` continuations, keyed by the numeric id of the
    /// continuation ref thing handed to Erlang.
    continuations: HashMap<u64, Continuation>,
    /// The next continuation id to hand out.
    next_continuation: u64,
    /// The live `ets:match_spec_compile/1` specs, keyed by the numeric id of the
    /// compiled-spec ref thing handed to Erlang. The value is the **raw** spec term
    /// the caller compiled, which is what [`beam_ets::match_spec_run`] consumes (see
    /// [`TableRegistry::insert_spec`] and the [`specs`] module docs).
    specs: HashMap<u64, Eterm>,
    /// The next compiled-spec id to hand out.
    next_spec: u64,
    /// The creation-time [`resolve::TableProfile`] of each live table, keyed by its
    /// handle. A profile is recorded only for a table whose
    /// `ets:info(T, write_concurrency)` value the model cannot express — the
    /// `beam_ets` [`TableInfo::write_concurrency`] boolean covers `false`/`true`, so
    /// the profile carries the atom `auto` and the hidden
    /// `{debug_hash_fixed_number_of_locks, N}` rendering (with `N` normalized).
    ///
    /// The entry **outlives a take/restore round trip**: every table-taking BIF
    /// takes its table through [`TableRegistry::take`] (which keeps the profile) and
    /// puts it back with [`restore_table`], so a later `ets:info/1,2` still sees the
    /// creation-time profile. Only a *permanent* removal drops the entry —
    /// [`TableRegistry::remove`] (which drops it) and the explicit
    /// [`TableRegistry::forget_profile`] `ets:delete/1` calls.
    profiles: HashMap<u64, resolve::TableProfile>,
}

impl TableRegistry {
    /// An empty registry.
    pub fn new() -> TableRegistry {
        TableRegistry {
            tables: HashMap::new(),
            names: HashMap::new(),
            continuations: HashMap::new(),
            next_continuation: CONTINUATION_ID_BASE,
            specs: HashMap::new(),
            next_spec: SPEC_ID_BASE,
            profiles: HashMap::new(),
        }
    }

    /// Inserts `table` under `handle`, returning the previous table if the handle
    /// was already present.
    ///
    /// **Continuations are not touched.** A continuation is an immutable value —
    /// the `beam_ets` traversal state plus the table identity it was issued against
    /// — exactly as in the oracle, where resuming one is repeatable and an
    /// intervening table operation does not destroy it. A continuation of a table
    /// that *has* gone away stays registered but can no longer resolve its table,
    /// so resuming it is `badarg` (see [`resume_continuation`]).
    pub fn insert(&mut self, handle: u64, table: Table) -> Option<Table> {
        self.tables.insert(handle, table)
    }

    /// The table stored under `handle`, if any.
    pub fn get(&self, handle: u64) -> Option<&Table> {
        self.tables.get(&handle)
    }

    /// The table stored under `handle`, mutably, if any.
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut Table> {
        self.tables.get_mut(&handle)
    }

    /// Whether `handle` names a table.
    pub fn contains(&self, handle: u64) -> bool {
        self.tables.contains_key(&handle)
    }

    /// Removes and returns the table stored under `handle`, if any, dropping every
    /// continuation that belonged to it **and its [`resolve::TableProfile`]**.
    ///
    /// This is the *permanent* removal (a whole-table delete): the profile is gone
    /// with the table. A caller that only borrows the table and puts it back must
    /// use [`TableRegistry::take`] instead, which keeps the profile so a later
    /// `ets:info/1,2` still reports the creation-time `write_concurrency` form.
    pub fn remove(&mut self, handle: u64) -> Option<Table> {
        self.continuations.retain(|_, cont| cont.table != handle);
        self.profiles.remove(&handle);
        self.tables.remove(&handle)
    }

    /// Removes and returns the table stored under `handle`, if any, keeping its
    /// [`resolve::TableProfile`].
    ///
    /// This is the take half of every table-taking BIF's borrow-and-restore round
    /// trip ([`take_table_gated`] → [`restore_table`]): the profile must survive so
    /// the table's `ets:info/1,2` rendering is unchanged after e.g. an `insert/2`.
    /// The profile is dropped only by [`TableRegistry::remove`] or an explicit
    /// [`TableRegistry::forget_profile`] (the whole-table `ets:delete/1`).
    ///
    /// **Continuations are not touched** (they are immutable values — see
    /// [`TableRegistry::insert`]): a take/restore round trip is invisible to them,
    /// so an unrelated table operation never invalidates a live `ets:select/3`
    /// continuation.
    pub fn take(&mut self, handle: u64) -> Option<Table> {
        self.tables.remove(&handle)
    }

    /// Drops the [`resolve::TableProfile`] recorded for `handle`, if any, leaving the
    /// table itself in place.
    ///
    /// `ets:delete/1` is the only permanent table removal that goes through the
    /// borrow-and-restore path ([`take_table_gated`], which keeps the profile), so it
    /// calls this to make sure a deleted handle leaves no lingering profile behind.
    pub fn forget_profile(&mut self, handle: u64) {
        self.profiles.remove(&handle);
    }

    /// Records `profile` as the creation-time [`resolve::TableProfile`] of the table
    /// under `handle`.
    ///
    /// Private: only [`new_2`] records a profile and only [`info_field`] reads one.
    fn insert_profile(&mut self, handle: u64, profile: resolve::TableProfile) {
        self.profiles.insert(handle, profile);
    }

    /// The creation-time [`resolve::TableProfile`] of the table under `handle`, if
    /// any — absent only for a table registered through [`TableRegistry::insert`]
    /// without one (a direct registry insertion, as in a test), for which
    /// [`info_field`] falls back to the value derived from [`TableInfo`].
    fn profile(&self, handle: u64) -> Option<resolve::TableProfile> {
        self.profiles.get(&handle).copied()
    }

    /// The number of live tables.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Whether the registry holds no tables.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Registers `name` as the handle of a table created with the `named_table`
    /// option, returning `false` (and changing nothing) when the name is already
    /// taken.
    ///
    /// A taken name makes `ets:new(Name, [named_table])` `badarg` in the oracle —
    /// the C `ets_new_2` fails when `insert_named_tab` finds the name — which keeps
    /// the name→handle mapping unambiguous for `ets:whereis/1`.
    pub fn insert_name(&mut self, name: Atom, handle: u64) -> bool {
        if self.names.contains_key(&name) {
            return false;
        }
        self.names.insert(name, handle);
        true
    }

    /// The handle of the `named_table` registered under `name`, if any (`None` is
    /// the oracle's `undefined`).
    pub fn lookup_name(&self, name: Atom) -> Option<u64> {
        self.names.get(&name).copied()
    }

    /// Removes `name` from the name registry, returning the handle it named.
    ///
    /// A deleted table leaves the oracle's meta name table (`remove_named_tab`),
    /// so its name is then free — `ets:whereis(Name)` is `undefined` and a fresh
    /// `ets:new(Name, [named_table])` succeeds.
    pub fn remove_name(&mut self, name: Atom) -> Option<u64> {
        self.names.remove(&name)
    }
    /// Registers `state` as a live continuation of the table stored under
    /// `table`, returning the continuation id (the id of the ref thing the
    /// `ets:select` BIFs hand to Erlang). `reverse` records the traversal's
    /// direction (see [`Continuation`]).
    ///
    /// A continuation is an **immutable value derived from the traversal state**
    /// (the `beam_ets` [`MatchContinuation`] is `PartialEq`), so an *equal* state
    /// already registered for the same table and direction reuses its id instead of
    /// minting a fresh one. That is what makes a resume **repeatable with an
    /// identical result term**: the oracle resumes one continuation any number of
    /// times and hands back `=:=`-equal chunks (continuations included — probed on
    /// erts-17.1: `{_,C} = ets:match_object(T,'_',2), ets:match_object(C) =:=
    /// ets:match_object(C)` ⇒ `true`).
    pub fn insert_continuation(
        &mut self,
        table: u64,
        state: MatchContinuation,
        reverse: bool,
    ) -> u64 {
        if let Some(existing) = self.continuations.iter().find_map(|(handle, cont)| {
            (cont.table == table && cont.reverse == reverse && cont.state == state).then_some(*handle)
        }) {
            return existing;
        }
        let handle = self.next_continuation;
        self.next_continuation += 1;
        self.continuations.insert(
            handle,
            Continuation {
                table,
                state,
                reverse,
            },
        );
        handle
    }

    /// Removes and returns the continuation stored under `handle`: the handle of
    /// its table, its opaque state and its direction (`None` when `handle` is
    /// unknown — the caller raises `badarg`).
    ///
    /// The continuation family no longer *consumes* on resume (see
    /// [`TableRegistry::continuation`]); this consuming accessor is kept for a host
    /// that permanently drops a traversal — [`TableRegistry::remove`], the
    /// whole-table delete, does the same for every continuation of a table.
    pub fn take_continuation(
        &mut self,
        handle: u64,
    ) -> Option<(u64, MatchContinuation, bool)> {
        self.continuations
            .remove(&handle)
            .map(|cont| (cont.table, cont.state, cont.reverse))
    }

    /// The continuation stored under `handle` — the handle of its table, a copy of
    /// its opaque state and its direction — or `None` when `handle` names no live
    /// continuation.
    ///
    /// The state is **cloned, never removed**: resuming a continuation is a pure
    /// function of it, so `ets:select(Cont)` is repeatable and an unrelated table
    /// operation cannot invalidate it (the oracle's continuation is an immutable
    /// value, contrary to a live cursor).
    pub fn continuation(&self, handle: u64) -> Option<(u64, MatchContinuation, bool)> {
        self.continuations
            .get(&handle)
            .map(|cont| (cont.table, cont.state.clone(), cont.reverse))
    }

    /// The handle of the table a live continuation belongs to, if any.
    ///
    /// [`resume_continuation`] needs it *before* resuming the continuation: the
    /// resuming call is access-gated as the calling process, and a denial must
    /// leave the continuation resumable (verified: after a non-owner's denied
    /// `ets:select(Cont)` on a `private` table, the owner can still resume it).
    pub fn continuation_table(&self, handle: u64) -> Option<u64> {
        self.continuations.get(&handle).map(|cont| cont.table)
    }

    /// Registers the spec term `raw` as a live compiled match spec, returning the
    /// fresh spec id (the id of the ref thing `ets:match_spec_compile/1` hands to
    /// Erlang).
    ///
    /// The **raw** term is stored — not a compiled form — because
    /// [`beam_ets::match_spec_run`] takes the raw clause list and compiles it
    /// itself; the compile in `match_spec_compile/1` is what validates the spec.
    /// See the [`specs`] module docs for the M1 substitution and its heap-lifetime
    /// caveat.
    ///
    /// Unlike the oracle, which garbage-collects an unreferenced compiled spec, an
    /// entry is never reclaimed here (no `ets` BIF releases a compiled spec), so a
    /// runtime that compiles specs in a loop grows this map for its lifetime — a
    /// documented M1 leak, not a wrong result.
    pub fn insert_spec(&mut self, raw: Eterm) -> u64 {
        let handle = self.next_spec;
        self.next_spec += 1;
        self.specs.insert(handle, raw);
        handle
    }

    /// The raw spec term registered under the compiled-spec `handle`, if any
    /// (`None` when the handle is unknown — the caller raises `badarg`, as for a
    /// forged reference).
    pub fn spec_raw(&self, handle: u64) -> Option<Eterm> {
        self.specs.get(&handle).copied()
    }

    /// Whether `handle` names a live compiled match spec (a `handle` produced by
    /// [`TableRegistry::insert_spec`]) — the `ets:is_compiled_ms/1` predicate.
    pub fn contains_spec(&self, handle: u64) -> bool {
        self.specs.contains_key(&handle)
    }

    /// Every live table as `(handle, named_table name)`, ascending by handle —
    /// the `ets:all/0` roster.
    ///
    /// The name is `Some` only for a table created with the `named_table` option
    /// (exactly the name registry's membership), which is the oracle's rule for
    /// rendering a tid as its name atom rather than its handle reference.
    pub(super) fn all_tids(&self) -> Vec<(u64, Option<Atom>)> {
        let mut ids: Vec<u64> = self.tables.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|id| {
                let name = self
                    .names
                    .iter()
                    .find(|(_, handle)| **handle == id)
                    .map(|(name, _)| *name);
                (id, name)
            })
            .collect()
    }

    /// Every live table handle owned by `owner`, **ascending by handle** — the
    /// owner→handle roster the runtime reclaims through on a process death
    /// (`beam_ets::Table::owner_died`).
    ///
    /// The table's owner is read from [`Table::owner`] (`Option<OwnerId>`); only a
    /// table whose owner is `Some(owner)` is listed, so an ownerless table (one
    /// inserted straight into the registry, as a test does) is never reported for
    /// any `owner`. The order is the ascending handle — the same order
    /// [`TableRegistry::all_tids`] uses — so a caller that mutates the registry
    /// while walking the roster (dropping a table, or re-keying it to a surviving
    /// heir) sees a deterministic sequence.
    ///
    /// `owner` comes from the [`TableRegistry::owner_of`] bridge, which encodes a
    /// `beam_process::ProcessId` exactly as [`resolve::caller_owner`] recorded it
    /// at `ets:new/2` time, so a pid read back here and one recorded there always
    /// compare equal.
    ///
    /// # Whole-roster, not just `named_table`s
    ///
    /// Unlike [`TableRegistry::all_tids`]'s name column, this roster covers an
    /// **unnamed** table too: its handle is the registry key, which a caller
    /// cannot derive from any public term, so this is the only way a host can
    /// enumerate (and therefore reclaim) the unnamed tables a dead owner left
    /// behind.
    pub fn handles_owned_by(&self, owner: OwnerId) -> Vec<u64> {
        let mut handles: Vec<u64> = self
            .tables
            .iter()
            .filter(|(_, table)| table.owner() == Some(owner))
            .map(|(handle, _)| *handle)
            .collect();
        handles.sort_unstable();
        handles
    }

    /// The `beam_ets` [`OwnerId`] of a `beam_process::ProcessId` — the pid→owner
    /// bridge a host needs to feed [`TableRegistry::handles_owned_by`] the key
    /// `ets:new/2` recorded.
    ///
    /// It is `OwnerId::new(pid.to_eterm().raw() as u64)`, a **lossless** function of
    /// the pid immediate and the same encoding [`resolve::owner_of`] (and
    /// `beam_rt`'s own `owner_key`) uses, so an owner read back from a table and a
    /// pid converted here always agree. Exposed as an associated function on the
    /// re-exported registry because `mod ets` is private: a free `pub fn` in
    /// [`resolve`] would not be reachable by a dependent crate.
    pub fn owner_of(pid: ProcessId) -> OwnerId {
        resolve::owner_of(pid)
    }
}

/// Registers the implemented `ets:*` BIFs (see the module docs for the scope).
pub(crate) fn register(reg: &mut BifRegistry) {
    // Creation and whole-table verbs.
    reg.register("ets", "new", 2, new_2);
    reg.register("ets", "delete", 1, delete_1);
    reg.register("ets", "delete_all_objects", 1, delete_all_objects_1);
    reg.register("ets", "info", 1, info_1);
    reg.register("ets", "info", 2, info_2);
    reg.register("ets", "tab2list", 1, tab2list_1);
    // Insertion.
    reg.register("ets", "insert", 2, insert_2);
    reg.register("ets", "insert_new", 2, insert_new_2);
    // Lookup and membership.
    reg.register("ets", "lookup", 2, lookup_2);
    reg.register("ets", "lookup_element", 3, lookup_element_3);
    reg.register("ets", "lookup_element", 4, lookup_element_4);
    reg.register("ets", "member", 2, member_2);
    reg.register("ets", "take", 2, take_2);
    // Removal by key/object.
    reg.register("ets", "delete", 2, delete_2);
    reg.register("ets", "delete_object", 2, delete_object_2);
    // Navigation.
    reg.register("ets", "first", 1, first_1);
    reg.register("ets", "last", 1, last_1);
    reg.register("ets", "next", 2, next_2);
    reg.register("ets", "prev", 2, prev_2);
    // Element updates.
    reg.register("ets", "update_counter", 3, update_counter_3);
    reg.register("ets", "update_counter", 4, update_counter_4);
    reg.register("ets", "update_element", 3, update_element_3);
    // Selection (compiled match specs) and the limit/continuation forms.
    reg.register("ets", "select", 1, select_1);
    reg.register("ets", "select", 2, select_2);
    reg.register("ets", "select", 3, select_3);
    // The match family and the count/delete verbs (child module `matches`).
    matches::register(reg);
    // The compiled match-spec family (child module `specs`).
    specs::register(reg);
    // The remaining faithful verbs (child module `extra`): the fix flag, the
    // name registry (`whereis`), `update_element/4` and `is_compiled_ms/1`.
    extra::register(reg);
    // The adopted verbs (child module `adopt`): the roster (`all/0`), the
    // ownership verbs (`setopts/2`, `give_away/3`) and the two fun-folds
    // (`foldl/3`, `foldr/3`).
    adopt::register(reg);
    // The key/navigation `*_lookup` family (child module `nav`).
    nav::register(reg);
    // The reverse select family (child module `reverse`).
    reverse::register(reg);
}

// --------------------------------------------------------------- creation

/// `ets:new(Name, Options)` — creates a table and returns its handle.
///
/// A `named_table` is created with the **name atom** as its handle (the oracle's
/// `ets:new(Name, [named_table])` returns the atom); every other table is created
/// with the handle ref thing. A `named_table` whose name is already registered is
/// `badarg` (the oracle's `ets_new_2` fails when the meta name table already holds
/// the name); every other table records its name field only and takes part in no
/// registry, matching `ets:whereis/1`'s `undefined` for it.
///
/// The calling process is recorded as the table owner. The parsed `{heir, …}`
/// option is set on the `TableOptions` before [`Table::new`], so the table
/// deep-copies it into table-owned storage at build time; a parsed
/// [`resolve::TableProfile`] (only present for a `write_concurrency` form the
/// model's boolean cannot express) is carried alongside the table for
/// `ets:info/1,2`.
fn new_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let name = args[0];
    if !is_atom(name) {
        return Err(badarg());
    }
    let resolve::ParsedOptions {
        mut options,
        profile,
    } = resolve::parse_options(ctx.atoms(), args[1])?;
    options.set_name(atom_val(name));
    // Read the name metadata before the options move into the table.
    let named = options.is_named();
    let table_name = options.name();

    let id = TableId::next();
    // `Table::new` installs the creation-time heir from the options (deep-copying
    // its gift data into table-owned storage).
    let mut table = Table::new(id, options)?;
    table.set_owner(Some(resolve::caller_owner(ctx)));
    {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        if named {
            // A `named_table` always carries the `Name` argument this call parsed
            // (see [`resolve::parse_options`]), so a missing one is a malformed
            // option set.
            let table_name = table_name.ok_or_else(badarg)?;
            if !tables.insert_name(table_name, id.raw()) {
                return Err(badarg());
            }
        }
        if let Some(profile) = profile {
            tables.insert_profile(id.raw(), profile);
        }
        tables.insert(id.raw(), table);
    }
    if named {
        Ok(make_atom(atom_val(name)))
    } else {
        make_ref_thing(ctx.heap(), id.raw())
    }
}

// ------------------------------------------------------------- whole-table

/// `ets:delete(Tab)` — deletes the whole table and returns `true`.
///
/// A deleted `named_table` also leaves the name registry (the oracle's
/// `remove_named_tab`), so its name becomes free and `ets:whereis/1` on it is
/// `undefined` again. The table is **write**-gated, and the gated take happens
/// before anything is removed, so a caller without write access gets a `badarg`
/// that leaves the table in place.
///
/// This is the only table-taking BIF that never restores its table, so it is also
/// the only one that drops the creation-time [`resolve::TableProfile`]
/// ([`TableRegistry::forget_profile`]): every other taker uses [`take_table_gated`],
/// which keeps the profile for the restore.
fn delete_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let name = if table.info().named_table {
        table.name()
    } else {
        None
    };
    let tables = ctx.tables().ok_or_else(unsupported_bif)?;
    tables.forget_profile(handle);
    if let Some(name) = name {
        tables.remove_name(name);
    }
    ctx.boolean(true)
}

/// `ets:delete_all_objects(Tab)` — empties the table and returns `true`.
fn delete_all_objects_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    table.delete_all_objects();
    restore_table(ctx, handle, table)?;
    ctx.boolean(true)
}

/// `ets:info(Tab)` — the table's introspection proplist, or the bare atom
/// `undefined` when `Tab` is a well-formed table argument that names no live table
/// (an unknown atom, the name of a table not created with `named_table`, or a
/// deleted table's handle). A `Tab` that is neither an atom nor a handle reference
/// is still `badarg`.
///
/// Never access-gated (the oracle lets a non-owner `info` a `private` table), so
/// the table is taken **without** an access check.
fn info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = match resolve::resolve_handle_opt(ctx, args[0])? {
        Some(handle) => handle,
        None => return ctx.intern_atom("undefined"),
    };
    let table = take_table(ctx, handle)?;
    let info = table.info();
    restore_table(ctx, handle, table)?;
    let profile = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.profile(handle)
    };

    let mut pairs = Vec::with_capacity(INFO_ITEMS.len());
    for item in INFO_ITEMS {
        let key = ctx.intern_atom(item)?;
        let value = info_field(ctx, &info, profile, item)?;
        pairs.push(ctx.tuple(&[key, value])?);
    }
    ctx.list(&pairs)
}

/// `ets:info(Tab, Item)` — one introspection field of a live table (an unknown
/// `Item` of a **live** table is `badarg`, matching the system `erl`).
///
/// The table is resolved **first**, so a `Tab` that names no live table answers the
/// bare atom `undefined` whatever `Item` is — including a non-atom one
/// (`ets:info(bogus, 42)` ⇒ `undefined`, while `ets:info(T, 42)` on a live `T` ⇒
/// `badarg`).
///
/// Never access-gated, like [`info_1`].
fn info_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = match resolve::resolve_handle_opt(ctx, args[0])? {
        Some(handle) => handle,
        None => return ctx.intern_atom("undefined"),
    };
    let item = args[1];
    if !is_atom(item) {
        return Err(badarg());
    }
    // The item name is copied out of the atom table before the table borrow, so
    // the registry access below does not overlap the immutable atom-table borrow.
    let name = ctx
        .atoms()
        .name(atom_val(item))
        .ok_or_else(badarg)?
        .to_string();

    let table = take_table(ctx, handle)?;
    let info = table.info();
    restore_table(ctx, handle, table)?;
    let profile = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.profile(handle)
    };
    info_field(ctx, &info, profile, &name)
}

/// `ets:tab2list(Tab)` — every object of the table, in iteration order.
fn tab2list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let objects = table.tab2list();
    restore_table(ctx, handle, table)?;
    copy_out_list(objects, ctx)
}

// --------------------------------------------------------------- insertion

/// The objects an `ets:insert/2` / `ets:insert_new/2` selection names.
///
/// A proper list *is* a list of objects — and `[]` (`NIL`) is the empty list, not
/// an object (the verified `erl` behaviour: `ets:insert(T, [])` is `true`); an
/// improper list is `badarg`; anything else is a single object, whose shape is
/// then validated by the table.
fn selection_objects(selection: Eterm) -> Result<Vec<Eterm>> {
    if is_list(selection) || is_nil(selection) {
        proper_list(selection).ok_or_else(badarg)
    } else {
        Ok(vec![selection])
    }
}

/// `ets:insert(Tab, ObjectOrList)` — inserts one object, or every object of a
/// proper list, and returns `true`.
fn insert_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let objects = selection_objects(args[1])?;

    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let mut failure = None;
    for object in objects {
        if let Err(err) = table.insert(object, ctx.atoms()) {
            failure = Some(err);
            break;
        }
    }
    restore_table(ctx, handle, table)?;
    match failure {
        Some(err) => Err(err),
        None => ctx.boolean(true),
    }
}

/// `ets:insert_new(Tab, ObjectOrList)` — inserts only when every key is absent,
/// atomically across a list.
///
/// The list semantics are the verified system `erl` (OTP 29 / erts-17.1): an
/// improper list or a non-tuple object is `badarg` (nothing inserted); a single
/// key already present makes the whole call `false` with **nothing** inserted;
/// duplicate keys *within* the list are allowed (the key is absent in the table
/// when the call starts) and the later object wins.
fn insert_new_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let objects = selection_objects(args[1])?;

    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let outcome = insert_new_into(&mut table, &objects, ctx.atoms());
    restore_table(ctx, handle, table)?;
    match outcome {
        Ok(inserted) => ctx.boolean(inserted),
        Err(err) => Err(err),
    }
}

/// The list-atomic body of `ets:insert_new/2` (see [`insert_new_2`]).
fn insert_new_into(table: &mut Table, objects: &[Eterm], atoms: &AtomTable) -> Result<bool> {    let keypos = table.keypos();
    // Validate every object and extract its key before touching the table, so a
    // bad element rejects the whole call.
    let mut keys = Vec::with_capacity(objects.len());
    for &object in objects {
        keys.push(key_of(object, keypos)?);
    }
    // Any key already present means "insert nothing".
    for &key in &keys {
        if table.member(key, atoms) {
            return Ok(false);
        }
    }
    for &object in objects {
        table.insert(object, atoms)?;
    }
    Ok(true)
}

// -------------------------------------------------------- lookup/membership

/// `ets:lookup(Tab, Key)` — the objects stored under `Key`, copied to the
/// caller's heap.
fn lookup_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];

    // The returned handles alias the table's (never-relocating) term storage, so
    // they stay valid after the table is restored; the registry borrow is dropped
    // before the copy so `ctx.heap()` can be used.
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let objects = table.lookup(key, ctx.atoms());
    restore_table(ctx, handle, table)?;

    copy_out_list(objects, ctx)
}

/// `ets:lookup_element(Tab, Key, Pos)` — element `Pos` (1-based) of the object
/// stored under `Key`.
///
/// The answer's *shape* depends on the table kind (verified against the system
/// `erl`, OTP 29 / erts-17.1): `set`/`ordered_set` store at most one object per
/// key, so the element is a **scalar**; `bag`/`duplicate_bag` hold several
/// objects under one key and the oracle answers a **list** of their `Pos`-th
/// elements (`ets:lookup_element(Bag, 1, 2)` ⇒ `[a, b]`), which is exactly what
/// [`Table::lookup_element_all`] returns. The multi kinds therefore route through
/// the list-returning primitive instead of the scalar one (which raises the typed
/// `unsupported` for a *present* key on a multi kind — a scalar answer would be a
/// wrong result).
///
/// The oracle's `badarg`s are preserved by both routes: `Pos == 0`, an absent
/// `Key`, and a *present* key whose object is shorter than `Pos` (for a multi kind
/// one short object makes the **whole** call `badarg`).
fn lookup_element_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let pos = element_pos(args[2])?;

    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    if table.kind().is_multi() {
        let outcome = table.lookup_element_all(key, pos, ctx.atoms());
        restore_table(ctx, handle, table)?;
        copy_out_list(outcome?, ctx)
    } else {
        let outcome = table.lookup_element(key, pos, ctx.atoms());
        restore_table(ctx, handle, table)?;
        copy_out(outcome?, ctx)
    }
}

/// `ets:lookup_element(Tab, Key, Pos, Default)` — as
/// [`lookup_element_3`](lookup_element_3), but an absent `Key` yields `Default`.
///
/// Independent of the kind, `Pos == 0` is `badarg` **before** the key is looked
/// at and an absent key yields `Default` whatever `Pos` is (even `Pos` beyond the
/// object's arity). A *present* key keeps
/// [`lookup_element_3`](lookup_element_3)'s failures and its kind-dependent shape:
/// the scalar `set`/`ordered_set` answer, or the `bag`/`duplicate_bag` element
/// **list**.
fn lookup_element_4(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let pos = element_pos(args[2])?;
    let default = args[3];

    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    if table.kind().is_multi() {
        let outcome = table.lookup_element_all_or(key, pos, default, ctx.atoms());
        restore_table(ctx, handle, table)?;
        match outcome? {
            // An absent key returns `default` verbatim (the caller's own term), so
            // there is nothing to copy out of table storage.
            None => Ok(default),
            Some(elements) => copy_out_list(elements, ctx),
        }
    } else {
        let outcome = table.lookup_element_or(key, pos, default, ctx.atoms());
        restore_table(ctx, handle, table)?;
        let value = outcome?;
        if value == default {
            // An absent key returns `default` verbatim (the caller's own term), so
            // there is nothing to copy out of table storage.
            Ok(default)
        } else {
            copy_out(value, ctx)
        }
    }
}

/// `ets:member(Tab, Key)` — whether `Key` is present.
fn member_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let present = table.member(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    ctx.boolean(present)
}

/// `ets:take(Tab, Key)` — removes `Key`'s entry and returns its objects (an
/// absent key yields the empty list, not an error).
fn take_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let outcome = table.take(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    copy_out_list(outcome?, ctx)
}

// ------------------------------------------------------------- removal

/// `ets:delete(Tab, Key)` — removes `Key`'s entry and returns `true` (an absent
/// key is not an error).
fn delete_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let outcome = table.delete(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    outcome?;
    ctx.boolean(true)
}

/// `ets:delete_object(Tab, Object)` — removes the one object equal to `Object`
/// (an absent key is not an error; a non-tuple object is `badarg`).
fn delete_object_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let object = args[1];
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let outcome = table.delete_object(object, ctx.atoms());
    restore_table(ctx, handle, table)?;
    outcome?;
    ctx.boolean(true)
}

// ------------------------------------------------------------ navigation

/// `ets:first(Tab)` — the first key in the table's iteration order.
fn first_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let key = table.first(ctx.atoms());
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, key)
}

/// `ets:last(Tab)` — the last key in the table's iteration order.
///
/// A **hash** kind (`set`/`bag`/`duplicate_bag`) has no backward walk, and the
/// oracle aliases `ets:last/1` to `ets:first/1` there — the C runs the *forward*
/// primitive for every non-`ordered_set` kind. Only an `ordered_set` reads the
/// real end of its order. This mirrors `nav::last_lookup_1` exactly (the two
/// verbs must never disagree). Verified against the system `erl` (OTP 29.1.1 /
/// erts-17.1): keys `[6,3,4,8,1]` in a `set` give `first` `6`/`last` `6`, while an
/// `ordered_set` gives `first` `1`/`last` `8`.
fn last_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let key = if table.kind().is_ordered() {
        table.last(ctx.atoms())
    } else {
        table.first(ctx.atoms())
    };
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, key)
}

/// `ets:next(Tab, Key)` — the key following `Key` (an `ordered_set` reads `Key`
/// as a range bound; the hash kinds require it present, else `badarg`).
///
/// An `ordered_set` additionally **rejects** the `'$end_of_table'` anchor with
/// `badarg` (see [`is_end_of_table_key`]) — the C `db_next_tree_common`'s
/// unconditional `DB_ERROR_BADKEY`; without it the anchor would be read as a
/// greatest-bound and answer `'$end_of_table'` where the oracle raises.
fn next_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    if table.kind().is_ordered() && is_end_of_table_key(ctx, key) {
        restore_table(ctx, handle, table)?;
        return Err(badarg());
    }
    let outcome = table.next(key, ctx.atoms());
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, outcome?)
}

/// `ets:prev(Tab, Key)` — the key preceding `Key` (mirrors [`next_2`]).
///
/// Mirroring [`last_1`]'s alias: a **hash** kind answers with the *forward* step
/// ([`Table::next`] — the oracle's behaviour: `prev` is aliased to `next` there),
/// only an `ordered_set` with the real predecessor. The chosen primitive's
/// `badarg` (a hash kind requires the anchor key to be present) propagates
/// unchanged. Verified against the system `erl` (OTP 29.1.1 / erts-17.1): keys
/// `[6,3,4,8,1]` give `next(T,3)` `4`/`prev(T,3)` `4` for a `set` and
/// `next(T,3)` `4`/`prev(T,3)` `1` for an `ordered_set`.
fn prev_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    if table.kind().is_ordered() && is_end_of_table_key(ctx, key) {
        restore_table(ctx, handle, table)?;
        return Err(badarg());
    }
    let outcome = if table.kind().is_ordered() {
        table.prev(key, ctx.atoms())
    } else {
        table.next(key, ctx.atoms())
    };
    restore_table(ctx, handle, table)?;
    key_or_end(ctx, outcome?)
}

// -------------------------------------------------------- element updates

/// `ets:update_counter(Tab, Key, Incr|Spec|List)` — adds the increments and
/// returns the new counter value(s).
fn update_counter_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let update_op = args[2];
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    // `Table::update_counter` needs the atom table *and* the caller heap at once;
    // the context lends them through two conflicting borrows, so it is given a
    // clone of the atom table (identical indices, and the call only reads it —
    // see `binconv::decode_term_bif` for the same pattern).
    let atoms = ctx.atoms().clone();
    let outcome = table.update_counter(key, update_op, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    outcome
}

/// `ets:update_counter(Tab, Key, Incr|Spec|List, Default)` — as
/// [`update_counter_3`], depositing `Default` (with its key replaced by `Key`)
/// when the key is absent.
fn update_counter_4(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let update_op = args[2];
    let default = args[3];
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.update_counter_with(key, update_op, default, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    outcome
}

/// `ets:update_element(Tab, Key, {Pos, Value} | [{Pos, Value}])` — replaces
/// element(s) of the object stored under `Key`, returning whether `Key` existed.
///
/// An absent `Key` is `false` **whatever the spec says** — the verified `erl`
/// behaviour answers the key question before it inspects the spec — so the spec is
/// only parsed once the key is known present.
fn update_element_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let key = args[1];
    let mut table = take_table_gated(ctx, handle, resolve::Gate::Write)?;
    let outcome = if table.member(key, ctx.atoms()) {
        update_element_of(&mut table, key, args[2], ctx)
    } else {
        Ok(false)
    };
    restore_table(ctx, handle, table)?;
    match outcome {
        Ok(updated) => ctx.boolean(updated),
        Err(err) => Err(err),
    }
}

/// Parses the `ets:update_element/3` spec and applies it to a present key.
fn update_element_of(
    table: &mut Table,
    key: Eterm,
    spec: Eterm,
    ctx: &mut BifContext<'_>,
) -> Result<bool> {
    let updates = parse_updates(spec)?;
    let atoms = ctx.atoms().clone();
    table.update_element(key, &updates, &atoms, ctx.heap())
}

/// The `{Pos, Value}` / `[{Pos, Value}, …]` element updates of
/// `ets:update_element/3` (a bare `{Pos, Value}` tuple or a proper list of them).
fn parse_updates(spec: Eterm) -> Result<Vec<(usize, Eterm)>> {
    let entries = if is_tuple(spec) {
        vec![spec]
    } else if is_list(spec) || is_nil(spec) {
        proper_list(spec).ok_or_else(badarg)?
    } else {
        return Err(badarg());
    };
    let mut updates = Vec::with_capacity(entries.len());
    for entry in entries {
        if !is_tuple(entry) || tuple_size(entry) != 2 {
            return Err(badarg());
        }
        let pos = element_pos(tuple_element(entry, 0))?;
        let value = tuple_element(entry, 1);
        updates.push((pos, value));
    }
    Ok(updates)
}

// ------------------------------------------------------------- selection

/// `ets:select(Tab, MatchSpec)` — the body values every object the compiled spec
/// selects produces, copied onto the caller's heap.
///
/// `MatchSpec` is a raw match-spec list compiled by [`beam_ets::compile`]; a spec
/// outside the supported subset is `badarg`.
fn select_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;

    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    // `Table::select` needs the atom table *and* the caller heap at once; the
    // context lends them through two conflicting borrows, so it is given a clone
    // of the atom table (identical indices, and the call only reads it — the same
    // pattern `update_counter` uses).
    let atoms = ctx.atoms().clone();
    let outcome = table.select(&spec, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    copy_out_list(outcome?, ctx)
}

/// `ets:select(Tab, MatchSpec, Limit)` — as [`select_2`], but stop after at most
/// `Limit` results (the oracle's chunking).
///
/// A reached limit yields a continuation, which [`select_1`] resumes; a traversal
/// that finishes first yields the results with no continuation. `Limit` is a
/// non-negative integer and `0` is `badarg` (the oracle raises it).
fn select_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let handle = resolve::resolve_handle(ctx, args[0])?;
    let spec = compile(args[1], ctx.atoms())?;
    // A non-integer or a negative limit is `badarg` here; `0` is rejected by
    // `select_limit` (matching the oracle).
    let limit = element_pos(args[2])?;

    let table = take_table_gated(ctx, handle, resolve::Gate::Read)?;
    let atoms = ctx.atoms().clone();
    let outcome = table.select_limit(&spec, limit, &atoms, ctx.heap());
    restore_table(ctx, handle, table)?;
    let (results, cont) = outcome?;
    finish_chunk(ctx, handle, results, cont, false)
}

/// `ets:select(Continuation)` — resumes a [`select_3`] (or `select_reverse/3`)
/// traversal.
///
/// The argument is a continuation ref thing returned by an earlier traversal (or
/// a resume); an unknown one is `badarg`.
///
/// The oracle treats `ets:select/1` and `ets:select_reverse/1` as **one** BIF that
/// follows the direction *stored in the continuation* (verified: resuming a
/// reverse continuation with `ets:select/1` walks backward, and resuming a forward
/// continuation with `ets:select_reverse/1` walks forward). Both entry points
/// therefore dispatch through [`dispatch_continuation`].
fn select_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    dispatch_continuation(ctx, args[0])
}

/// The shared `ets:select/1` / `ets:select_reverse/1` dispatcher: it resumes the
/// continuation through `Table::select_reverse_continue` when the mapper minted it
/// on the reverse path and through `Table::select_continue` otherwise, so the walk
/// follows the **stored** direction (the oracle's one-BIF rule). An argument that
/// is no continuation at all is `badarg`.
pub(super) fn dispatch_continuation(
    ctx: &mut BifContext<'_>,
    cont_term: Eterm,
) -> Result<Eterm> {
    resume_continuation(ctx, cont_term, |table, cont, reverse, atoms, heap| {
        if reverse {
            table.select_reverse_continue(cont, atoms, heap)
        } else {
            table.select_continue(cont, atoms, heap)
        }
    })
}

/// Resumes the continuation named by `cont_term` with `resume` and renders the
/// resulting chunk.
///
/// Shared by every `/1` entry point of the select family (`ets:select/1`,
/// `ets:match/1`, `ets:match_object/1` — see the child module [`matches`]): the
/// continuation registry is table-scoped, and the originating `/3` call stored the
/// compiled spec inside the state, so the resume needs nothing but the opaque
/// continuation term. An unknown continuation is `badarg`.
///
/// The stored **direction** is handed to `resume` (so the select dispatcher picks
/// the right `beam_ets` primitive) and to [`finish_chunk`] (so a minted follow-up
/// chunk keeps the direction).
///
/// The resume is **read**-gated as the calling process against the continuation's
/// table (`erl_db.c`'s `ets_select_1` calls `db_get_table` with `DB_READ`), and the
/// check runs *before* the state is read, so a denied resume gives the oracle's
/// `badarg` and leaves the traversal resumable.
///
/// **The resume never consumes the continuation.** The registry entry is read
/// (cloned) and left in place, so the same continuation may be resumed any number
/// of times with an identical result — the oracle's continuation is an immutable
/// value, not a live cursor, and an unrelated table operation does not invalidate
/// it. A continuation whose table has since been deleted (or whose `named_table`
/// name was renamed away) is still `badarg`: the table lookup / the `beam_ets`
/// identity check inside `resume` reject it.
pub(super) fn resume_continuation(
    ctx: &mut BifContext<'_>,
    cont_term: Eterm,
    resume: impl FnOnce(&Table, &MatchContinuation, bool, &AtomTable, &mut Heap) -> Result<Traversal>,
) -> Result<Eterm> {
    let handle = ref_thing_id(cont_term)?;
    let owner = Some(resolve::caller_owner(ctx));
    {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        let table_handle = tables.continuation_table(handle).ok_or_else(badarg)?;
        let table = tables.get(table_handle).ok_or_else(badarg)?;
        resolve::Gate::Read.check(table, owner)?;
    }
    let (table_handle, cont, reverse) = {
        let tables = ctx.tables().ok_or_else(unsupported_bif)?;
        tables.continuation(handle).ok_or_else(badarg)?
    };

    let table = take_table(ctx, table_handle)?;
    let atoms = ctx.atoms().clone();
    let outcome = resume(&table, &cont, reverse, &atoms, ctx.heap());
    restore_table(ctx, table_handle, table)?;
    let (results, next) = outcome?;
    finish_chunk(ctx, table_handle, results, next, reverse)
}

/// Renders one chunk of the select/match family exactly as the oracle (erts-17.1)
/// does:
///
/// * a produced continuation ⇒ the 2-tuple `{Results, ContTerm}`, where
///   `ContTerm` is a fresh continuation ref thing;
/// * the traversal finished ⇒ the 2-tuple `{Results, '$end_of_table'}` — or the
///   **bare** atom `'$end_of_table'` when `Results` is empty (verified against the
///   system `erl`: a 4-object table with `limit` 2 renders the final chunk as the
///   bare atom while a 5-object table renders `{[{5,a}], '$end_of_table'}`).
///
/// `reverse` is the direction the minted continuation carries (see [`Continuation`]);
/// the forward callers (`ets:select/3`, `ets:match/3`, `ets:match_object/3`) pass
/// `false`, the reverse path true.
pub(super) fn finish_chunk(
    ctx: &mut BifContext<'_>,
    table: u64,
    results: Vec<Eterm>,
    cont: Option<MatchContinuation>,
    reverse: bool,
) -> Result<Eterm> {
    match cont {
        Some(state) => {
            let handle = {
                let tables = ctx.tables().ok_or_else(unsupported_bif)?;
                tables.insert_continuation(table, state, reverse)
            };
            let cont_term = make_ref_thing(ctx.heap(), handle)?;
            let list = copy_out_list(results, ctx)?;
            ctx.tuple(&[list, cont_term])
        }
        None if results.is_empty() => ctx.intern_atom(END_OF_TABLE),
        None => {
            let list = copy_out_list(results, ctx)?;
            let end = ctx.intern_atom(END_OF_TABLE)?;
            ctx.tuple(&[list, end])
        }
    }
}

/// The `(results, continuation)` pair a chunked traversal step returns — the shape
/// of every `beam_ets` `*_limit` / `*_continue` method.
pub(super) type Traversal = (Vec<Eterm>, Option<MatchContinuation>);

// ----------------------------------------------------------------- helpers

/// Removes the table under `handle`, or `badarg` when it is unknown.
///
/// This is the **ungated** take: it performs no access check and exists only for
/// `ets:info/1,2`, which the oracle never gates. Every other table-taking BIF uses
/// [`take_table_gated`].
pub(super) fn take_table(ctx: &mut BifContext<'_>, handle: u64) -> Result<Table> {
    take_table_gated(ctx, handle, resolve::Gate::Ungated)
}

/// Removes the table under `handle` after enforcing `gate` for the **calling**
/// process ([`resolve::caller_owner`]), or `badarg` when the table is unknown or
/// the caller lacks the required access.
///
/// The gate is evaluated against the table while it is still in the registry, so a
/// denial leaves the table where it is (the oracle's `badarg` never destroys it).
///
/// The take goes through [`TableRegistry::take`], **not**
/// [`TableRegistry::remove`]: the table is only borrowed for the duration of the
/// BIF and put back with [`restore_table`], so its creation-time
/// [`resolve::TableProfile`] must survive the round trip (else `ets:info/1,2` would
/// report `write_concurrency = false` / `decentralized_counters = false` for every
/// table that had ever been touched). Only `ets:delete/1`, the one non-restoring
/// caller, drops the profile explicitly ([`TableRegistry::forget_profile`]).
pub(in crate::ets) fn take_table_gated(
    ctx: &mut BifContext<'_>,
    handle: u64,
    gate: resolve::Gate,
) -> Result<Table> {
    let owner = Some(resolve::caller_owner(ctx));
    let tables = ctx.tables().ok_or_else(unsupported_bif)?;
    {
        let table = tables.get(handle).ok_or_else(badarg)?;
        gate.check(table, owner)?;
    }
    tables.take(handle).ok_or_else(badarg)
}

/// Puts `table` back under `handle`.
pub(super) fn restore_table(ctx: &mut BifContext<'_>, handle: u64, table: Table) -> Result<()> {
    let tables = ctx.tables().ok_or_else(unsupported_bif)?;
    tables.insert(handle, table);
    Ok(())
}

/// Copies `term` — which may alias table-owned storage — onto the caller's heap.
pub(super) fn copy_out(term: Eterm, ctx: &mut BifContext<'_>) -> Result<Eterm> {
    beam_message::copy_struct_simple(term, ctx.heap())
}

/// Copies every object of a table-aliasing vector onto the caller's heap and
/// returns them as a list.
pub(super) fn copy_out_list(objects: Vec<Eterm>, ctx: &mut BifContext<'_>) -> Result<Eterm> {
    let mut copied = Vec::with_capacity(objects.len());
    for object in objects {
        copied.push(beam_message::copy_struct_simple(object, ctx.heap())?);
    }
    ctx.list(&copied)
}

/// The `ets:first/1`/`last/1`/`next/2`/`prev/2` result: the key, or the
/// `'$end_of_table'` atom when the navigation is exhausted.
pub(super) fn key_or_end(ctx: &mut BifContext<'_>, key: Option<Eterm>) -> Result<Eterm> {
    match key {
        Some(key) => copy_out(key, ctx),
        None => ctx.intern_atom(END_OF_TABLE),
    }
}

/// Whether `key` is the atom `'$end_of_table'` — the anchor an **`ordered_set`**
/// `next`/`prev`/`next_lookup`/`prev_lookup` rejects with `badarg`.
///
/// The C `db_next_tree_common`/`db_prev_tree_common`
/// (`../../erts/emulator/beam/erl_db_tree.c:654,729`) return `DB_ERROR_BADKEY`
/// unconditionally on that atom — even when it is a *stored* key of the tree — so
/// the four ordered walkers must raise `badarg` instead of reading it as a range
/// bound. A hash kind does **not** special-case it (`db_next_hash_common` merely
/// requires the anchor be present), so this predicate is consulted only for an
/// `ordered_set`; a hash kind whose stored key really is `'$end_of_table'` keeps
/// working.
pub(super) fn is_end_of_table_key(ctx: &BifContext<'_>, key: Eterm) -> bool {
    is_atom(key) && ctx.atoms().name(atom_val(key)) == Some(END_OF_TABLE)
}

/// Whether `table`'s interior implements a genuine **backward** key walk.
///
/// Only an `ordered_set` does. The hash interiors (`set`/`bag`/`duplicate_bag`)
/// define no `last_key`/`prev_key`, so the oracle **aliases** the reverse
/// navigators to the forward ones for them — `ets:last/1` ≡ `ets:first/1`,
/// `ets:prev/2` ≡ `ets:next/2`, `ets:last_lookup/1` ≡ `ets:first_lookup/1` and
/// `ets:prev_lookup/2` ≡ `ets:next_lookup/2` (pinned in [`nav`]).
///
/// This is the single home of that rule: the `nav` bridge and the `adopt` fun-folds
/// both key off it, and `beam_ets::Table::last`/`prev` are left alone so the
/// interior keeps its own semantics.
pub(super) fn has_backward_walk(table: &Table) -> bool {
    table.kind().is_ordered()
}

/// The 1-based element position argument of `lookup_element`/`update_element`,
/// reused as the `ets:select/3`-family **limit** (a non-integer or a negative
/// value is `badarg`; `0` is rejected downstream by [`Table`]).
pub(super) fn element_pos(term: Eterm) -> Result<usize> {
    if !is_small(term) {
        return Err(badarg());
    }
    let pos = signed_value(term);
    if pos < 0 {
        return Err(badarg());
    }
    Ok(pos as usize)
}

/// The value of one `ets:info/1,2` field, or `badarg` for an item the table does
/// not know (the verified system `erl` behaviour: `ets:info(T, bogus_item)`
/// raises `badarg`).
///
/// This reproduces [`TableInfo::entries`] field-for-field — [`entries`] cannot be
/// called directly because it needs a `&mut AtomTable` while a BIF may only borrow
/// the atom table immutably (`ctx.atoms()`), so the field-name and value atoms are
/// instead interned through [`BifContext::intern_atom`]. `id` deliberately differs
/// from [`TableInfo::entries`]' numeric id: it is the table's **tid reference** —
/// the same [`crate::access::make_ref_thing`] handle `ets:new/2` hands out for an
/// unnamed table, so `is_reference(ets:info(T, id))` holds exactly as in the
/// oracle. `node` is always `nonode@nohost` (the runtime is never distributed).
///
/// `owner` and `heir` render the recorded pid terms ([`resolve::owner_pid`] of the
/// table's [`TableInfo::owner`]/[`TableInfo::heir`]); an unset owner is the atom
/// `undefined` and an unset heir (or one whose recorded owner bridges to no pid)
/// is `none`. `decentralized_counters` is the value the model resolved
/// ([`TableInfo::decentralized_counters`]); `write_concurrency` uses the
/// creation-time `profile` for the forms the model's boolean cannot express (the
/// atom `auto` and the hidden lock-count tuple) and the boolean otherwise.
///
/// [`entries`]: TableInfo::entries
fn info_field(
    ctx: &mut BifContext<'_>,
    info: &TableInfo,
    profile: Option<resolve::TableProfile>,
    item: &str,
) -> Result<Eterm> {
    match item {
        // The oracle's `id` is the table's tid *reference* (`#Ref<…>`), and it is
        // the very term `ets:new/2` hands out for an unnamed table (verified:
        // `ets:info(T, id) =:= T` ⇒ `true`). `TableInfo::id` is the raw numeric id
        // (the registry key), so it is re-wrapped into the handle ref thing rather
        // than returned as a small int.
        "id" => make_ref_thing(ctx.heap(), info.id.raw()),
        // The `decentralized_counters` value is the one `beam_ets` resolved at
        // creation (`TableOptions::decentralized_counters`, whose rule the model
        // owns): `{write_concurrency, auto}` sets the counters flag even for a
        // `set`, while `{write_concurrency, true}` on a `set` does not, and the
        // resolved value is carried on the snapshot.
        "decentralized_counters" => ctx.boolean(info.decentralized_counters),
        "read_concurrency" => ctx.boolean(info.read_concurrency),
        // The model's boolean cannot distinguish `true` from `auto` or from the
        // hidden lock-count tuple, so a profile (recorded by `ets:new/2` for those
        // two forms only) wins when present; a table inserted directly through the
        // registry has no profile and falls back to the boolean.
        "write_concurrency" => match profile {
            Some(profile) => write_concurrency_term(ctx, profile),
            None => ctx.boolean(info.write_concurrency),
        },
        "compressed" => ctx.boolean(info.compressed),
        "memory" => ctx.int(info.memory as i64),
        // The owner pid `ets:new/2` recorded (an ownerless table — one inserted
        // directly through the registry — renders `undefined`).
        "owner" => match info.owner.and_then(resolve::owner_pid) {
            Some(pid) => Ok(pid),
            None => ctx.intern_atom("undefined"),
        },
        // The installed heir pid, or the atom `none` when there is none (a fresh
        // table's heir is `none`).
        "heir" => match info.heir.and_then(resolve::owner_pid) {
            Some(pid) => Ok(pid),
            None => ctx.intern_atom("none"),
        },
        "name" => match info.name {
            Some(atom) => Ok(make_atom(atom)),
            None => ctx.intern_atom("undefined"),
        },
        "size" => ctx.int(info.size as i64),
        // The runtime is never distributed.
        "node" => ctx.intern_atom("nonode@nohost"),
        "named_table" => ctx.boolean(info.named_table),
        "type" => ctx.intern_atom(info.kind.as_atom_name()),
        "keypos" => ctx.int(info.keypos as i64),
        "protection" => ctx.intern_atom(info.access.as_atom_name()),
        _ => Err(badarg()),
    }
}

/// The term of the `ets:info(T, write_concurrency)` value the model's
/// [`TableInfo::write_concurrency`] boolean cannot express
/// ([`resolve::TableProfile`]): the atom `auto`, or the hidden lock-count tuple
/// `{debug_hash_fixed_number_of_locks, N}` (whose count
/// [`resolve::parse_options`] already normalized). The plain `false`/`true`
/// forms are rendered by the caller from the boolean.
fn write_concurrency_term(
    ctx: &mut BifContext<'_>,
    form: resolve::TableProfile,
) -> Result<Eterm> {
    match form {
        resolve::TableProfile::Auto => ctx.intern_atom("auto"),
        resolve::TableProfile::FixedLocks(n) => {
            let tag = ctx.intern_atom("debug_hash_fixed_number_of_locks")?;
            let count = ctx.int(n as i64)?;
            ctx.tuple(&[tag, count])
        }
    }
}

/// Runs one `-eval` statement under the system `erl` and returns its stdout with
/// the trailing newlines trimmed — the oracle cross-check the child modules'
/// tests share (`nav`/`reverse` call `super::super::oracle`).
///
/// `None` means "no oracle here" (no `erl` on `PATH`, or the oracle itself
/// failed); the calling test then **skips loudly** (the `tests/match_spec.rs`
/// pattern), so the golden it pins is never silently unchecked.
#[cfg(test)]
pub(super) fn oracle(statement: &str) -> Option<String> {
    let output = std::process::Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\n', '\r'])
                .to_string(),
        ),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_ets::TableOptions;
    use beam_process::ProcessId;
    use beam_term::NIL;

    use crate::access::{is_reference, is_tuple, ref_thing_id, tuple_element, tuple_size};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            // The names the original four-BIF tests use (kept first so their
            // indices do not move).
            "true",
            "false",
            "ok",
            "set",
            "ordered_set",
            "bag",
            "keypos",
            "public",
            "named_table",
            "mytab",
            "a",
            "b",
            "not_an_option",
            // Names the expanded BIF set needs.
            "undefined",
            "duplicate_bag",
            "protected",
            "private",
            END_OF_TABLE,
            "id",
            "name",
            "type",
            "protection",
            "size",
            "memory",
            "owner",
            "compressed",
            "read_concurrency",
            "write_concurrency",
            // The remaining 15-field `ets:info/1` names/values.
            "decentralized_counters",
            "heir",
            "none",
            "node",
            "nonode@nohost",
            // The match-spec variables the `ets:select` specs use.
            "$1",
            "$2",
            "c",
            "p",
            "q",
            "z",
            // The `{write_concurrency, auto}` option value.
            "auto",
        ]);
        atoms
    }

    fn table(id: u64) -> Table {
        Table::new(TableId::new(id), TableOptions::new()).expect("test table")
    }

    /// The per-test `(heap, atoms, registry)` a `BifContext` is built over.
    ///
    /// Returned as three owned values (rather than a struct holding the context)
    /// so a test keeps the shared `&atoms` borrow available alongside `ctx` — the
    /// `true`/`false`/name assertions read the atom table directly.
    fn parts() -> (Heap, AtomTable, TableRegistry) {
        (
            Heap::new(65536).expect("test heap"),
            seeded_atoms(),
            TableRegistry::new(),
        )
    }    /// Creates a `set` table in `ctx` and returns its handle.
    fn new_table(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
        let name = ctx.atom(name).expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        new_2(ctx, &[name, opts]).expect("ets:new")
    }

    /// Creates a table of `kind` in `ctx` and returns its handle (like
    /// [`new_table`], which fixes the `set` kind).
    fn new_table_of(ctx: &mut BifContext<'_>, name: &str, kind: &str) -> Eterm {
        let name = ctx.atom(name).expect("seeded");
        let kind = ctx.atom(kind).expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        new_2(ctx, &[name, opts]).expect("ets:new")
    }

    /// Builds the 2-tuple `{key, value}` in `ctx`.
    fn pair(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
        let k = ctx.atom(key).expect("seeded");
        let v = ctx.int(value).expect("fits");
        ctx.tuple(&[k, v]).expect("fits")
    }

    fn is_true(atoms: &AtomTable, term: Eterm) -> bool {
        atoms.name(atom_val(term)) == Some("true")
    }

    fn is_false(atoms: &AtomTable, term: Eterm) -> bool {
        atoms.name(atom_val(term)) == Some("false")
    }

    /// Whether `term` is the atom `undefined`.
    fn is_undefined(atoms: &AtomTable, term: Eterm) -> bool {
        atoms.name(atom_val(term)) == Some("undefined")
    }
    #[test]
    fn insert_get_and_remove_round_trip() {
        let mut reg = TableRegistry::new();
        assert!(reg.is_empty());

        assert!(reg.insert(7, table(1)).is_none());
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(7));
        assert!(reg.get(7).is_some());
        assert!(reg.get_mut(7).is_some());
        assert!(reg.get(8).is_none());

        assert!(reg.remove(7).is_some());
        assert!(reg.is_empty());
        assert!(reg.remove(7).is_none());
    }

    #[test]
    fn insert_replaces_and_returns_the_previous_table() {
        let mut reg = TableRegistry::new();
        reg.insert(1, table(1));
        assert!(reg.insert(1, table(2)).is_some());
        assert_eq!(reg.len(), 1);
    }

    /// The owner→handle roster lists each owner apart, **ascending by handle**,
    /// and never reports an ownerless table for any owner.
    #[test]
    fn the_owner_roster_is_per_owner_and_ascending() {
        let alpha = OwnerId::new(0x11);
        let beta = OwnerId::new(0x22);
        let mut reg = TableRegistry::new();
        // Inserted out of handle order so the sort is observable.
        for (handle, owner) in [
            (30u64, Some(alpha)),
            (10, Some(alpha)),
            (20, Some(beta)),
            (5, None),
        ] {
            let mut t = table(handle);
            t.set_owner(owner);
            reg.insert(handle, t);
        }

        assert_eq!(reg.handles_owned_by(alpha), vec![10, 30]);
        assert_eq!(reg.handles_owned_by(beta), vec![20]);
        // A non-owner gets an empty roster — including the raw owner id of the
        // ownerless table's (absent) owner.
        assert!(reg.handles_owned_by(OwnerId::new(0x33)).is_empty());
        assert!(reg.handles_owned_by(OwnerId::new(5)).is_empty());
    }

    /// A dead owner's tables are reclaimed through the roster and the **existing**
    /// removal path: the roster hands back the handles, and `remove` (+
    /// `remove_name`) drops them.
    #[test]
    fn the_roster_withdrawal_uses_the_existing_remove_path() {
        let alpha = OwnerId::new(0x11);
        let mut reg = TableRegistry::new();
        let mut t = table(40);
        t.set_owner(Some(alpha));
        reg.insert(40, t);
        assert!(reg.insert_name(Atom::from_raw(1), 40), "name registered");

        // The runtime lists the dead owner's handles...
        let owned = reg.handles_owned_by(alpha);
        assert_eq!(owned, vec![40]);
        // ...and drops each through the existing removal path.
        for handle in owned {
            assert!(reg.remove(handle).is_some());
            reg.remove_name(Atom::from_raw(1));
        }
        assert!(reg.is_empty());
        assert!(reg.handles_owned_by(alpha).is_empty());
        assert!(reg.lookup_name(Atom::from_raw(1)).is_none());
    }

    /// `owner_of` round-trips the pid immediate, and the roster finds the tables
    /// `ets:new/2` recorded for that pid.
    #[test]
    fn owner_of_round_trips_a_pid_and_the_roster_finds_its_tables() {
        let alpha = ProcessId::new(1, 1);
        let beta = ProcessId::new(1, 2);
        for pid in [alpha, beta, ProcessId::new(0, 0), ProcessId::new(255, 7)] {
            assert_eq!(
                TableRegistry::owner_of(pid).raw(),
                pid.to_eterm().raw() as u64,
                "the pid→owner bridge is lossless for {pid:?}"
            );
        }

        // Two contexts with different `self_pid`s over one registry: each context
        // creates its tables as *its* process.
        let mut heap = Heap::new(65536).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let alpha_handles: Vec<u64> = {
            let mut ctx = BifContext::new(&mut heap, &atoms, alpha).with_tables(&mut reg);
            let h1 = new_table(&mut ctx, "mytab");
            let h2 = new_table(&mut ctx, "mytab");
            vec![
                ref_thing_id(h1).expect("handle"),
                ref_thing_id(h2).expect("handle"),
            ]
        };
        let beta_handle = {
            let mut ctx = BifContext::new(&mut heap, &atoms, beta).with_tables(&mut reg);
            ref_thing_id(new_table(&mut ctx, "mytab")).expect("handle")
        };

        let mut expected = alpha_handles.clone();
        expected.sort_unstable();
        assert_eq!(
            reg.handles_owned_by(TableRegistry::owner_of(alpha)),
            expected,
            "alpha's two tables, ascending"
        );
        assert_eq!(
            reg.handles_owned_by(TableRegistry::owner_of(beta)),
            vec![beta_handle]
        );
        assert!(reg
            .handles_owned_by(TableRegistry::owner_of(ProcessId::new(3, 9)))
            .is_empty());
    }    /// An `auto` profile, for the profile-lifetime tests (the only non-boolean
    /// form besides the hidden lock count).
    fn auto_profile() -> resolve::TableProfile {
        resolve::TableProfile::Auto
    }

    /// A table take/**restore** round trip — what every table-taking BIF performs
    /// ([`take_table_gated`] → [`restore_table`]) — keeps the creation-time profile;
    /// only a *permanent* removal ([`TableRegistry::remove`]) drops it.
    #[test]
    fn the_profile_survives_a_take_but_not_a_remove() {
        let mut reg = TableRegistry::new();
        let profile = auto_profile();
        reg.insert(3, table(3));
        reg.insert_profile(3, profile);
        assert_eq!(reg.profile(3), Some(profile));

        // `take` is the borrow half: the profile stays, and the restore finds it.
        let taken = reg.take(3).expect("the table");
        assert_eq!(reg.profile(3), Some(profile), "take dropped the profile");
        reg.insert(3, taken);
        assert_eq!(reg.profile(3), Some(profile), "restore lost the profile");

        // `remove` is the permanent removal: the profile goes with the table.
        assert!(reg.remove(3).is_some());
        assert_eq!(reg.profile(3), None, "remove kept the profile");
    }

    /// [`TableRegistry::forget_profile`] drops only the profile — the table itself
    /// stays put (it is the `ets:delete/1` step that runs *after* the gated take).
    #[test]
    fn forget_profile_drops_only_the_profile() {
        let mut reg = TableRegistry::new();
        reg.insert(4, table(4));
        reg.insert_profile(4, auto_profile());
        reg.forget_profile(4);
        assert_eq!(reg.profile(4), None);
        assert!(reg.contains(4), "the table itself is untouched");
    }
    #[test]
    fn default_is_empty() {
        assert!(TableRegistry::default().is_empty());
    }

    #[test]
    fn new_insert_lookup_delete_round_trip() {
        let mut heap = Heap::new(8192).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let name = ctx.atom("mytab").expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");

        let a = ctx.atom("a").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let object = ctx.tuple(&[a, one]).expect("fits");

        let inserted = insert_2(&mut ctx, &[handle, object]).expect("ets:insert");
        assert_eq!(atoms.name(atom_val(inserted)), Some("true"));

        let found = lookup_2(&mut ctx, &[handle, a]).expect("ets:lookup");
        let elems = proper_list(found).expect("a proper list");
        assert_eq!(elems.len(), 1);
        let got = elems[0];
        assert!(is_tuple(got));
        assert_eq!(tuple_size(got), 2);
        assert_eq!(tuple_element(got, 0), a);
        assert_eq!(tuple_element(got, 1), one);

        // The key is absent before insertion / after a different key.
        let b = ctx.atom("b").expect("seeded");
        let missing = lookup_2(&mut ctx, &[handle, b]).expect("ets:lookup");
        assert_eq!(proper_list(missing).expect("a proper list").len(), 0);

        let deleted = delete_1(&mut ctx, &[handle]).expect("ets:delete");
        assert_eq!(atoms.name(atom_val(deleted)), Some("true"));

        // After `delete/1` the handle no longer names a table.
        assert!(matches!(
            lookup_2(&mut ctx, &[handle, a]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn insert_accepts_a_list_of_objects() {
        let mut heap = Heap::new(8192).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let name = ctx.atom("mytab").expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");

        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let obj_a = ctx.tuple(&[a, one]).expect("fits");
        let obj_b = ctx.tuple(&[b, two]).expect("fits");
        let objects = ctx.list(&[obj_a, obj_b]).expect("fits");

        assert!(insert_2(&mut ctx, &[handle, objects]).is_ok());

        let found = lookup_2(&mut ctx, &[handle, a]).expect("ets:lookup");
        let elems = proper_list(found).expect("a proper list");
        assert_eq!(elems.len(), 1);
        assert_eq!(tuple_element(elems[0], 1), one);
    }

    #[test]
    fn new_rejects_a_non_atom_name_and_bad_options() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        // Name must be an atom.
        let name_int = ctx.int(1).expect("fits");
        let kind = ctx.atom("set").expect("seeded");
        let valid_opts = ctx.list(&[kind]).expect("fits");
        assert!(matches!(
            new_2(&mut ctx, &[name_int, valid_opts]),
            Err(BeamError::BadArg)
        ));

        // An unknown option atom is `badarg`.
        let name = ctx.atom("mytab").expect("seeded");
        let bogus = ctx.atom("not_an_option").expect("seeded");
        let bad_opts = ctx.list(&[bogus]).expect("fits");
        assert!(matches!(
            new_2(&mut ctx, &[name, bad_opts]),
            Err(BeamError::BadArg)
        ));

        // A `{keypos, N}` option is accepted (and `N = 0` is rejected).
        let keypos = ctx.atom("keypos").expect("seeded");
        let two = ctx.int(2).expect("fits");
        let keypos_tuple = ctx.tuple(&[keypos, two]).expect("fits");
        let keypos_opts = ctx.list(&[keypos_tuple]).expect("fits");
        assert!(new_2(&mut ctx, &[name, keypos_opts]).is_ok());

        let zero = ctx.int(0).expect("fits");
        let zero_tuple = ctx.tuple(&[keypos, zero]).expect("fits");
        let zero_opts = ctx.list(&[zero_tuple]).expect("fits");
        assert!(matches!(
            new_2(&mut ctx, &[name, zero_opts]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn handles_that_are_not_refs_are_badarg() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut reg = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let one = ctx.int(1).expect("fits");
        let a = ctx.atom("a").expect("seeded");
        let object = ctx.tuple(&[a, one]).expect("fits");
        // A small integer is not a table handle.
        assert!(matches!(
            insert_2(&mut ctx, &[one, object]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            lookup_2(&mut ctx, &[one, a]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(delete_1(&mut ctx, &[one]), Err(BeamError::BadArg)));
        assert!(matches!(
            delete_2(&mut ctx, &[one, a]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            member_2(&mut ctx, &[one, a]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            info_1(&mut ctx, &[one]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            first_1(&mut ctx, &[one]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn ets_without_a_registry_is_unsupported() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let name = ctx.atom("mytab").expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");

        match new_2(&mut ctx, &[name, opts]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }

    #[test]
    fn delete_key_and_object_remove_entries() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        // `delete/2` removes the key and reports `true` (even when absent).
        assert!(is_true(&atoms, delete_2(&mut ctx, &[handle, a]).expect("delete/2")));
        assert!(matches!(
            lookup_2(&mut ctx, &[handle, a]),
            Ok(list) if proper_list(list).expect("list").is_empty()
        ));
        assert!(is_true(
            &atoms,
            delete_2(&mut ctx, &[handle, a]).expect("delete absent")
        ));

        // `delete_object/2` removes one object; a non-tuple object is `badarg`.
        let obj_again = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_again]).expect("insert");
        assert!(is_true(
            &atoms,
            delete_object_2(&mut ctx, &[handle, obj_again]).expect("delete_object")
        ));
        let one = ctx.int(1).expect("fits");
        assert!(matches!(
            delete_object_2(&mut ctx, &[handle, one]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn insert_new_is_atomic_across_a_list() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");

        let p1 = pair(&mut ctx, "p", 1);
        let p2 = pair(&mut ctx, "p", 2);
        // A single new object is `true`.
        assert!(is_true(&atoms, insert_new_2(&mut ctx, &[handle, p1]).expect("insert_new")));
        // The same key again is `false`, and the stored object is unchanged.
        assert!(is_false(&atoms, insert_new_2(&mut ctx, &[handle, p2]).expect("insert_new")));
        let p = ctx.atom("p").expect("seeded");
        let stored = lookup_2(&mut ctx, &[handle, p]).expect("lookup");
        assert_eq!(tuple_element(proper_list(stored).expect("list")[0], 1), ctx.int(1).unwrap());

        // A list with one already-present key inserts nothing.
        let q1 = pair(&mut ctx, "q", 1);
        let p5 = pair(&mut ctx, "p", 5);
        let mixed = ctx.list(&[q1, p5]).expect("fits");
        assert!(is_false(&atoms, insert_new_2(&mut ctx, &[handle, mixed]).expect("insert_new")));
        let q = ctx.atom("q").expect("seeded");
        assert!(matches!(
            lookup_2(&mut ctx, &[handle, q]),
            Ok(list) if proper_list(list).expect("list").is_empty()
        ));

        // An empty list is `true`; an improper list and a non-tuple are `badarg`.
        assert!(is_true(
            &atoms,
            insert_new_2(&mut ctx, &[handle, NIL]).expect("insert_new []")
        ));
        let improper = ctx.cons(q1, q1).expect("fits");
        assert!(matches!(
            insert_new_2(&mut ctx, &[handle, improper]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            insert_new_2(&mut ctx, &[handle, q]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn member_lookup_element_and_take() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 7);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        assert!(is_true(&atoms, member_2(&mut ctx, &[handle, a]).expect("member")));
        assert!(is_false(&atoms, member_2(&mut ctx, &[handle, b]).expect("member")));

        // `lookup_element/3` returns element 2 of `{a, 7}`.
        let two = ctx.int(2).expect("fits");
        let element = lookup_element_3(&mut ctx, &[handle, a, two]).expect("lookup_element");
        assert_eq!(element, ctx.int(7).unwrap());

        // An out-of-range position and a non-integer position are `badarg`.
        let nine = ctx.int(9).expect("fits");
        assert!(matches!(
            lookup_element_3(&mut ctx, &[handle, a, nine]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            lookup_element_3(&mut ctx, &[handle, a, a]),
            Err(BeamError::BadArg)
        ));
        // `lookup_element/4` yields the default for an absent key.
        let default = ctx.int(99).expect("fits");
        let value = lookup_element_4(&mut ctx, &[handle, b, two, default]).expect("lookup_element/4");
        assert_eq!(value, default);

        // `take/2` removes the entry and returns the objects.
        let taken = take_2(&mut ctx, &[handle, a]).expect("take");
        let elems = proper_list(taken).expect("a proper list");
        assert_eq!(elems.len(), 1);
        assert_eq!(tuple_element(elems[0], 1), ctx.int(7).unwrap());
        assert!(is_false(&atoms, member_2(&mut ctx, &[handle, a]).expect("member")));
    }

    #[test]
    fn navigation_walks_the_keys() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        // An empty set: `first`/`last` are `'$end_of_table'`.
        let eot = first_1(&mut ctx, &[handle]).expect("first");
        assert_eq!(atoms.name(atom_val(eot)), Some(END_OF_TABLE));

        // One key: `first` and `last` agree, `next`/`prev` are the terminator.
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");
        let a = ctx.atom("a").expect("seeded");
        assert_eq!(first_1(&mut ctx, &[handle]).expect("first"), a);
        assert_eq!(last_1(&mut ctx, &[handle]).expect("last"), a);
        let next = next_2(&mut ctx, &[handle, a]).expect("next");
        assert_eq!(atoms.name(atom_val(next)), Some(END_OF_TABLE));
        let prev = prev_2(&mut ctx, &[handle, a]).expect("prev");
        assert_eq!(atoms.name(atom_val(prev)), Some(END_OF_TABLE));

        // A key that is not present is `badarg` for the hash kinds.
        let b = ctx.atom("b").expect("seeded");
        assert!(matches!(
            next_2(&mut ctx, &[handle, b]),
            Err(BeamError::BadArg)
        ));
    }

    /// The `'$end_of_table'` **anchor** is `badarg` for the `ordered_set` walkers
    /// `ets:next/2` and `ets:prev/2` (the `*_lookup` pair is pinned in
    /// [`super::nav`]) — even when that atom is itself a **stored** key, and even
    /// on an empty table.
    ///
    /// The C `db_next_tree_common`/`db_prev_tree_common`
    /// (`../../erts/emulator/beam/erl_db_tree.c:654,729`) return `DB_ERROR_BADKEY`
    /// unconditionally on `am_EOT`, so the atom is never read as a range bound. A
    /// hash kind has no such special-case (`db_next_hash_common`), so there the
    /// atom is an ordinary key.
    ///
    /// Oracle probe (`erl -noshell`, OTP 29.1.1 / erts-17.1), an `ordered_set`
    /// holding `{1,1},{2,2},{3,3}`: `ets:next(T,'$end_of_table')` and
    /// `ets:prev(T,'$end_of_table')` both raise `badarg`; storing
    /// `'$end_of_table'` as a key leaves both `badarg`; an empty `ordered_set` is
    /// `badarg` too.
    #[test]
    fn ordered_set_next_and_prev_reject_the_end_of_table_anchor() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table_of(&mut ctx, "mytab", "ordered_set");
        let eot = ctx.atom(END_OF_TABLE).expect("seeded");

        // An empty `ordered_set` rejects the anchor as well.
        assert!(matches!(
            next_2(&mut ctx, &[handle, eot]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            prev_2(&mut ctx, &[handle, eot]),
            Err(BeamError::BadArg)
        ));

        // Three integer-keyed objects: an *absent* anchor that is not the walk's
        // terminator is still rejected (the C checks the atom before any range
        // logic), while the ordinary walks are unaffected.
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let three = ctx.int(3).expect("fits");
        let a = ctx.atom("a").expect("seeded");
        for key in [one, two, three] {
            let obj = ctx.tuple(&[key, a]).expect("fits");
            insert_2(&mut ctx, &[handle, obj]).expect("insert");
        }
        assert_eq!(next_2(&mut ctx, &[handle, one]).expect("next(1)"), two);
        assert_eq!(prev_2(&mut ctx, &[handle, three]).expect("prev(3)"), two);
        assert!(matches!(
            next_2(&mut ctx, &[handle, eot]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            prev_2(&mut ctx, &[handle, eot]),
            Err(BeamError::BadArg)
        ));

        // The atom **stored** as a key: the anchor is rejected even though it is
        // present in the tree (the C rejects `am_EOT` before it ever looks it up).
        let eot_obj = ctx.tuple(&[eot, eot]).expect("fits");
        insert_2(&mut ctx, &[handle, eot_obj]).expect("insert");
        assert!(matches!(
            next_2(&mut ctx, &[handle, eot]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            prev_2(&mut ctx, &[handle, eot]),
            Err(BeamError::BadArg)
        ));

        // Every rejection restored the table: the real walk still answers.
        assert_eq!(next_2(&mut ctx, &[handle, one]).expect("still live"), two);

        // A hash kind is unaffected: with the atom stored it is an ordinary key,
        // so `ets:next/2` walks from it and `ets:prev/2` aliases that step.
        let hashed = new_table_of(&mut ctx, "mytab", "set");
        for obj in [
            ctx.tuple(&[one, a]).expect("fits"),
            ctx.tuple(&[eot, a]).expect("fits"),
        ] {
            insert_2(&mut ctx, &[hashed, obj]).expect("insert");
        }
        let next = next_2(&mut ctx, &[hashed, eot]).expect("next(eot) on a set is not badarg");
        let prev = prev_2(&mut ctx, &[hashed, eot]).expect("prev(eot) on a set is not badarg");
        assert!(
            beam_term::compare::eq(next, prev, &atoms),
            "a hash kind aliases prev to next"
        );
    }

    #[test]
    fn tab2list_and_delete_all_objects() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let obj_a = pair(&mut ctx, "a", 1);
        let obj_b = pair(&mut ctx, "b", 2);
        let objects = ctx.list(&[obj_a, obj_b]).expect("fits");
        insert_2(&mut ctx, &[handle, objects]).expect("insert");

        let listed = tab2list_1(&mut ctx, &[handle]).expect("tab2list");
        assert_eq!(proper_list(listed).expect("list").len(), 2);

        assert!(is_true(
            &atoms,
            delete_all_objects_1(&mut ctx, &[handle]).expect("delete_all_objects")
        ));
        let listed = tab2list_1(&mut ctx, &[handle]).expect("tab2list");
        assert!(proper_list(listed).expect("list").is_empty());
    }

    #[test]
    fn info_reports_the_table_fields() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        let size_item = ctx.atom("size").expect("seeded");
        let size = info_2(&mut ctx, &[handle, size_item]).expect("info/2 size");
        assert_eq!(size, ctx.int(1).unwrap());

        let type_item = ctx.atom("type").expect("seeded");
        let kind = info_2(&mut ctx, &[handle, type_item]).expect("info/2 type");
        assert_eq!(atoms.name(atom_val(kind)), Some("set"));

        let name_item = ctx.atom("name").expect("seeded");
        let name = info_2(&mut ctx, &[handle, name_item]).expect("info/2 name");
        assert_eq!(atoms.name(atom_val(name)), Some("mytab"));

        // `info/2` `id` is the table's *tid reference* — the very handle ref thing
        // `ets:new/2` handed out, so it reads back to the same registry key (the
        // oracle: `ets:info(T, id) =:= T` ⇒ `true`, `is_reference/1` ⇒ `true`).
        let id_item = ctx.atom("id").expect("seeded");
        let id = info_2(&mut ctx, &[handle, id_item]).expect("info/2 id");
        assert!(is_reference(id), "info/2 `id` is not a reference");
        assert_eq!(
            ref_thing_id(id).expect("id is a ref thing"),
            ref_thing_id(handle).expect("handle id")
        );
        assert!(beam_term::compare::eq(id, handle, &atoms), "id =:= T");
        // The full proplist has one pair per `INFO_ITEMS` entry — the oracle's 15
        // fields, in the oracle's order.
        assert_eq!(INFO_ITEMS.len(), 15);
        let proplist = info_1(&mut ctx, &[handle]).expect("info/1");
        let pairs = proper_list(proplist).expect("a proper list");
        assert_eq!(pairs.len(), INFO_ITEMS.len());
        for (pair, item) in pairs.iter().zip(INFO_ITEMS) {
            let name = ctx
                .atoms()
                .name(atom_val(tuple_element(*pair, 0)))
                .expect("a field-name atom");
            assert_eq!(name, *item, "info/1 field order mismatch");
        }

        // `heir` is `none` on a fresh table and `node` is `nonode@nohost`; a
        // `set` reports `decentralized_counters => false`.
        let heir_item = ctx.atom("heir").expect("seeded");
        let heir = info_2(&mut ctx, &[handle, heir_item]).expect("info/2 heir");
        assert_eq!(ctx.atoms().name(atom_val(heir)), Some("none"));
        let node_item = ctx.atom("node").expect("seeded");
        let node = info_2(&mut ctx, &[handle, node_item]).expect("info/2 node");
        assert_eq!(ctx.atoms().name(atom_val(node)), Some("nonode@nohost"));
        let dc_item = ctx.atom("decentralized_counters").expect("seeded");
        let dc = info_2(&mut ctx, &[handle, dc_item]).expect("info/2 decentralized_counters");
        assert!(is_false(&atoms, dc));

        // An unknown item is `badarg` (the verified `erl` behaviour).
        let bogus = ctx.atom("z").expect("seeded");
        assert!(matches!(
            info_2(&mut ctx, &[handle, bogus]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            info_2(&mut ctx, &[handle, size]),
            Err(BeamError::BadArg)
        ));
    }

    /// The effective `write_concurrency` / `decentralized_counters` creation-time
    /// profile survives the take/restore round trip every table-taking BIF performs,
    /// so `ets:info/2` keeps reporting what `ets:new/2` computed even after an
    /// `insert/2` + `lookup/2` (the oracle: `[set,{write_concurrency,auto}]` reports
    /// `{auto,true}`).
    #[test]
    fn info_keeps_the_profile_across_table_taking_bifs() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        let kind = ctx.atom("set").expect("seeded");
        let wc_key = ctx.atom("write_concurrency").expect("seeded");
        let auto = ctx.atom("auto").expect("seeded");
        let wc_auto = ctx.tuple(&[wc_key, auto]).expect("fits");
        let opts = ctx.list(&[kind, wc_auto]).expect("fits");
        let name = ctx.atom("mytab").expect("seeded");
        let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");

        // Two table-taking BIFs in between — each takes the table out of the
        // registry and puts it back.
        let obj = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj]).expect("insert");
        let key = ctx.atom("a").expect("seeded");
        lookup_2(&mut ctx, &[handle, key]).expect("lookup");

        let item = ctx.atom("write_concurrency").expect("seeded");
        let wc = info_2(&mut ctx, &[handle, item]).expect("info/2 write_concurrency");
        assert_eq!(ctx.atoms().name(atom_val(wc)), Some("auto"));
        let dc_item = ctx.atom("decentralized_counters").expect("seeded");
        let dc = info_2(&mut ctx, &[handle, dc_item]).expect("info/2 decentralized_counters");
        assert!(is_true(&atoms, dc), "decentralized_counters");
    }

    /// `ets:lookup_element/3,4` over the four kinds, pinned against the live oracle
    /// (`erts-17.1`).
    ///
    /// The matrix (objects `{a,b}` then `{a,c}` inserted): `set`/`ordered_set` keep
    /// only the last object, so element 2 is the **scalar** `c`; `bag`/
    /// `duplicate_bag` hold both, so element 2 is the **list** `[b,c]`. An absent
    /// key with a default returns that default **verbatim** for every kind, and
    /// `Pos == 0`, an absent key without a default and a present object shorter
    /// than `Pos` are `badarg` everywhere (measured: the same `{error,badarg}` for
    /// all four kinds).
    #[test]
    fn lookup_element_matrix_matches_the_live_oracle() {
        let script = concat!(
            "S = ets:new(s,[set]), B = ets:new(b,[bag]), ",
            "D = ets:new(d,[duplicate_bag]), O = ets:new(o,[ordered_set]), ",
            "[ets:insert(T,{a,X}) || T <- [S,B,D,O], X <- [b,c]], ",
            "io:format(\"~p~n~p~n\", ",
            "[[ets:lookup_element(T,a,2) || T <- [S,B,D,O]], ",
            "[[ets:lookup_element(T,z,2,def2), ets:lookup_element(T,z,9,def9)] ",
            "|| T <- [S,B,D,O]]])"
        );
        let golden = concat!(
            "[c,[b,c],[b,c],c]\n",
            "[[def2,def9],[def2,def9],[def2,def9],[def2,def9]]"
        );
        if let Some(actual) = oracle(script) {
            assert_eq!(actual, golden, "the system `erl` no longer matches the goldens");
        }

        for (kind, multi) in [
            ("set", false),
            ("ordered_set", false),
            ("bag", true),
            ("duplicate_bag", true),
        ] {
            let (mut heap, atoms, mut reg) = parts();
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
            let name = ctx.atom("mytab").expect("seeded");
            let kind_atom = ctx.atom(kind).expect("seeded");
            let opts = ctx.list(&[kind_atom]).expect("fits");
            let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");

            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let obj_b = ctx.tuple(&[a, b]).expect("fits");
            let obj_c = ctx.tuple(&[a, c]).expect("fits");
            insert_2(&mut ctx, &[handle, obj_b]).expect("insert");
            insert_2(&mut ctx, &[handle, obj_c]).expect("insert");

            let two = ctx.int(2).expect("fits");
            let got = lookup_element_3(&mut ctx, &[handle, a, two]).expect("lookup_element/3");
            if multi {
                let elems = proper_list(got).expect("a multi kind answers a list");
                assert_eq!(elems.len(), 2, "{kind}: both objects");
                assert!(elems.contains(&b) && elems.contains(&c), "{kind}: {{b, c}}");
            } else {
                assert!(is_atom(got), "{kind}: a single object answers a scalar");
                assert_eq!(got, c, "{kind}: the last object wins");
            }

            // `Pos == 0`, an absent key without a default, and a present object
            // shorter than `Pos` are all `badarg` — for every kind.
            let zero = ctx.int(0).expect("fits");
            assert!(matches!(
                lookup_element_3(&mut ctx, &[handle, a, zero]),
                Err(BeamError::BadArg)
            ));
            let z = ctx.atom("z").expect("seeded");
            assert!(matches!(
                lookup_element_3(&mut ctx, &[handle, z, two]),
                Err(BeamError::BadArg)
            ));
            let four = ctx.int(4).expect("fits");
            assert!(matches!(
                lookup_element_3(&mut ctx, &[handle, a, four]),
                Err(BeamError::BadArg)
            ));

            // `/4`: an absent key returns the caller's default **verbatim**, at any
            // position — including one past the object's arity.
            let def = ctx.tuple(&[a, obj_b]).expect("fits");
            for pos in [two, ctx.int(9).expect("fits")] {
                let absent =
                    lookup_element_4(&mut ctx, &[handle, z, pos, def]).expect("lookup_element/4");
                assert!(beam_term::compare::eq(absent, def, &atoms), "{kind}: the default");
            }
            // …while `Pos == 0` is `badarg` even with a default, and a *present*
            // short object keeps `lookup_element/3`'s failure.
            assert!(matches!(
                lookup_element_4(&mut ctx, &[handle, z, zero, def]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                lookup_element_4(&mut ctx, &[handle, a, four, def]),
                Err(BeamError::BadArg)
            ));
        }

        // Mixed arities under one key: a `bag` holding `{a,b}` and `{a,c,d}` is
        // `badarg` at `Pos == 3` (one object is shorter), while a `set` — which
        // keeps only `{a,c,d}` — answers `d` (measured: `[d,{error,badarg}]`).
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let c = ctx.atom("c").expect("seeded");
        let three = ctx.int(3).expect("fits");
        for (kind, expected) in [("set", Some(c)), ("bag", None)] {
            let name = ctx.atom("mytab").expect("seeded");
            let kind_atom = ctx.atom(kind).expect("seeded");
            let opts = ctx.list(&[kind_atom]).expect("fits");
            let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");
            let short = ctx.tuple(&[a, b]).expect("fits");
            let long = ctx.tuple(&[a, b, c]).expect("fits");
            insert_2(&mut ctx, &[handle, short]).expect("insert");
            insert_2(&mut ctx, &[handle, long]).expect("insert");
            match expected {
                Some(value) => {
                    let got = lookup_element_3(&mut ctx, &[handle, a, three]).expect("pos 3");
                    assert_eq!(got, value, "{kind}: element 3 of the surviving object");
                }
                None => assert!(
                    matches!(
                        lookup_element_3(&mut ctx, &[handle, a, three]),
                        Err(BeamError::BadArg)
                    ),
                    "{kind}: one short object makes the whole call badarg"
                ),
            }
            delete_1(&mut ctx, &[handle]).expect("delete");
        }
    }

    /// `ets:info(T, decentralized_counters)` — the field is the value `beam_ets`
    /// resolved at creation (`TableOptions::decentralized_counters`), not a
    /// re-derivation from `type`/`write_concurrency`: `{write_concurrency, auto}`
    /// on a `set` reports `true` while `type` is `set` (the oracle's rule).
    ///
    /// Each case is re-probed against the live oracle, so the golden cannot rot.
    #[test]
    fn decentralized_counters_matches_the_live_oracle() {
        // (option list source, expected value) — measured on OTP 29.1.1 / erts-17.1.
        let cases: [(&str, bool); 9] = [
            ("[set]", false),
            ("[set,{write_concurrency,true}]", false),
            ("[ordered_set,{write_concurrency,true}]", true),
            ("[set,{write_concurrency,auto}]", true),
            ("[ordered_set]", false),
            ("[set,{decentralized_counters,true},{write_concurrency,true}]", true),
            ("[set,{write_concurrency,false}]", false),
            ("[private,set,{write_concurrency,true}]", false),
            ("[ordered_set,{decentralized_counters,false},{write_concurrency,true}]", false),
        ];
        for (options, expected) in cases {
            let script = format!(
                "T = ets:new(t, {options}), \
                 io:format(\"~p~n\",[ets:info(T, decentralized_counters)])"
            );
            if let Some(actual) = oracle(&script) {
                assert_eq!(
                    actual,
                    if expected { "true" } else { "false" },
                    "the oracle disagrees for {options}"
                );
            }
        }

        // The same nine option lists through this crate's `ets:new/2` + `ets:info/2`.
        for (options, expected) in cases {
            let (mut heap, atoms, mut reg) = parts();
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
            let name = ctx.atom("mytab").expect("seeded");
            let opts = option_list(&mut ctx, options);
            let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");
            let item = ctx.atom("decentralized_counters").expect("seeded");
            let dc = info_2(&mut ctx, &[handle, item]).expect("info/2");
            if expected {
                assert!(is_true(&atoms, dc), "{options}: expected true");
            } else {
                assert!(is_false(&atoms, dc), "{options}: expected false");
            }
        }

        // A table inserted **without** going through `ets:new/2` still reports the
        // value `beam_ets` resolved on the snapshot: `decentralized_counters` reads
        // `TableInfo` directly (the model owns the rule), so no profile is needed.
        let mut reg = TableRegistry::new();
        let mut opts = TableOptions::new();
        opts.set_kind(beam_ets::TableKind::OrderedSet)
            .set_write_concurrency(true);
        let mut table = Table::new(TableId::new(99), opts).expect("test table");
        table.set_owner(None);
        reg.insert(99, table);
        let (mut heap, atoms) = (Heap::new(4096).expect("test heap"), seeded_atoms());
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = make_ref_thing(ctx.heap(), 99).expect("handle");
        let item = ctx.atom("decentralized_counters").expect("seeded");
        let dc = info_2(&mut ctx, &[handle, item]).expect("info/2 of a profile-less table");
        assert!(
            is_true(&atoms, dc),
            "a fine-locked ordered_set snapshot reports the resolved field"
        );
    }

    /// Builds the [`Eterm`] option list for a small source list such as
    /// `[private,set,{write_concurrency,true}]` (the `ets:new/2` shapes the
    /// `decentralized_counters` matrix pins; commas inside `{...}` do not split).
    fn option_list(ctx: &mut BifContext<'_>, source: &str) -> Eterm {
        let inner = source.trim_start_matches('[').trim_end_matches(']');
        let mut parts: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut depth = 0usize;
        for ch in inner.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    current.push(ch);
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    current.push(ch);
                }
                ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
                _ => current.push(ch),
            }
        }
        if !current.is_empty() {
            parts.push(current);
        }

        let mut elems = Vec::new();
        for part in parts {
            let part = part.trim();
            if let Some(tuple) = part.strip_prefix('{') {
                let tuple = tuple.trim_end_matches('}');
                let (key, value) = tuple.split_once(',').expect("{key,value}");
                let key = ctx.atom(key.trim()).expect("seeded");
                let value = match value.trim() {
                    "true" => ctx.atom("true").expect("seeded"),
                    "false" => ctx.atom("false").expect("seeded"),
                    "auto" => ctx.atom("auto").expect("seeded"),
                    other => ctx.int(other.parse().expect("an integer")).expect("fits"),
                };
                elems.push(ctx.tuple(&[key, value]).expect("fits"));
            } else if !part.is_empty() {
                elems.push(ctx.atom(part).expect("seeded"));
            }
        }
        ctx.list(&elems).expect("fits")
    }

    /// The `option_list` builder produces the shapes the oracle probes use.
    #[test]
    fn option_list_builds_the_oracle_option_shapes() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let cases: [(&str, usize); 7] = [
            ("[set]", 1),
            ("[set,{write_concurrency,true}]", 2),
            ("[ordered_set]", 1),
            ("[set,{write_concurrency,auto}]", 2),
            ("[set,{decentralized_counters,true},{write_concurrency,true}]", 3),
            ("[set,{write_concurrency,false}]", 2),
            ("[private,set,{write_concurrency,true}]", 3),
        ];
        for (source, element_count) in cases {
            let list = option_list(&mut ctx, source);
            assert_eq!(
                proper_list(list).expect("a proper list").len(),
                element_count,
                "{source}"
            );
        }
    }    /// The bare `set`/`protected` atoms are **no-ops**: they never override an
    /// earlier kind/access (the C `else if (val == am_set || val == am_protected) ;`,
    /// `erl_db.c:2710`), unlike every other bare atom.
    #[test]
    fn bare_set_and_protected_options_do_not_override() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        // (options, expected type, expected protection)
        let cases: [(&[&str], &str, &str); 6] = [
            (&["ordered_set", "set"], "ordered_set", "protected"),
            (&["bag", "set"], "bag", "protected"),
            (&["private", "protected"], "set", "private"),
            (&["public", "protected"], "set", "public"),
            (&["set"], "set", "protected"),
            (&["protected"], "set", "protected"),
        ];
        for (i, (options, kind, protection)) in cases.iter().enumerate() {
            let mut elems = Vec::new();
            for option in *options {
                elems.push(ctx.atom(option).expect("seeded"));
            }
            let opts = ctx.list(&elems).expect("fits");
            let name = ctx.atom("mytab").expect("seeded");
            let handle = new_2(&mut ctx, &[name, opts]).expect("ets:new");

            let type_item = ctx.atom("type").expect("seeded");
            let got = info_2(&mut ctx, &[handle, type_item]).expect("info/2 type");
            assert_eq!(ctx.atoms().name(atom_val(got)), Some(*kind), "case {i}");
            let prot_item = ctx.atom("protection").expect("seeded");
            let got = info_2(&mut ctx, &[handle, prot_item]).expect("info/2 protection");
            assert_eq!(
                ctx.atoms().name(atom_val(got)),
                Some(*protection),
                "case {i}"
            );
        }
    }
    /// `ets:info/1,2` on a *well-formed* table argument that names no live table is
    /// the atom `undefined` — an unknown atom, an atom naming a table that is not a
    /// `named_table`, and a deleted table's handle alike — for **both** arities and
    /// whatever the `Item`. A term of any other shape is `badarg`, and a *live* table
    /// with an unknown/non-atom item is `badarg` too. (The whole matrix was probed
    /// live on erts-17.1; the only deviation is the bare-`make_ref()` case noted in
    /// `resolve::resolve_handle_opt`.)
    #[test]
    fn info_of_a_table_that_names_nothing_is_undefined() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);

        // A table created *without* `named_table`: the name atom is only its internal
        // `name` field, so `ets:info(mytab)` is `undefined` in the oracle while the
        // handle reference resolves.
        let handle = new_table(&mut ctx, "mytab");
        let unnamed = ctx.atom("mytab").expect("seeded");
        // A name no table has ever used.
        let unknown = ctx.atom("z").expect("seeded");
        let name_item = ctx.atom("name").expect("seeded");
        let one = ctx.int(1).expect("fits");

        // The live table is unaffected.
        let full = info_1(&mut ctx, &[handle]).expect("info/1 of a live table");
        assert_eq!(proper_list(full).expect("15 fields").len(), INFO_ITEMS.len());

        for (tab, item) in [
            (unknown, name_item),
            (unknown, one),
            (unnamed, name_item),
            (unnamed, one),
        ] {
            let full = info_1(&mut ctx, &[tab]).expect("info/1 names no table");
            assert!(is_undefined(&atoms, full), "info/1 of a table-less name");
            let single = info_2(&mut ctx, &[tab, item]).expect("info/2 names no table");
            assert!(is_undefined(&atoms, single), "info/2 of a table-less name");
        }

        // A deleted table. `ets:new(Name, [named_table])` returns the name atom, so
        // the handle *is* that atom; a non-`named_table` handle is a reference.
        let named = {
            let name = ctx.atom("b").expect("seeded");
            let kind = ctx.atom("set").expect("seeded");
            let named_table = ctx.atom("named_table").expect("seeded");
            let opts = ctx.list(&[kind, named_table]).expect("fits");
            new_2(&mut ctx, &[name, opts]).expect("ets:new(named_table)")
        };
        let live_named = info_1(&mut ctx, &[named]).expect("info/1 of a live named table");
        assert_eq!(proper_list(live_named).expect("15 fields").len(), INFO_ITEMS.len());
        let ref_handle = new_table(&mut ctx, "c");
        assert!(is_true(&atoms, delete_1(&mut ctx, &[named]).expect("delete named")));
        assert!(is_true(&atoms, delete_1(&mut ctx, &[ref_handle]).expect("delete ref")));
        for (tab, item) in [
            (named, name_item),
            (named, one),
            (ref_handle, name_item),
            (ref_handle, one),
        ] {
            let full = info_1(&mut ctx, &[tab]).expect("info/1 of a deleted table");
            assert!(is_undefined(&atoms, full), "info/1 of a deleted table");
            let single = info_2(&mut ctx, &[tab, item]).expect("info/2 of a deleted table");
            assert!(is_undefined(&atoms, single), "info/2 of a deleted table");
        }

        // A hand-built ref thing whose id is in no registry: the M1 deviation (a bare
        // `make_ref()` is `badarg` in the oracle, which can tell a tid reference from
        // an ordinary one; the M1 handle cannot).
        let stray = make_ref_thing(ctx.heap(), 4242).expect("fits");
        assert!(is_undefined(
            &atoms,
            info_1(&mut ctx, &[stray]).expect("info/1 stray ref")
        ));
        assert!(is_undefined(
            &atoms,
            info_2(&mut ctx, &[stray, name_item]).expect("info/2 stray ref")
        ));

        // Everything else is `badarg` for both arities: a small int, a tuple, `[]`
        // and a pid (`ets:info(42)` / `ets:info({t,Node})` / `ets:info([])` /
        // `ets:info(self())` all raise `badarg` in the oracle).
        let key = ctx.atom("mytab").expect("seeded");
        let tuple = ctx.tuple(&[key, one]).expect("fits");
        let pid = ProcessId::new(1, 0).to_eterm();
        for tab in [one, tuple, NIL, pid] {
            assert!(
                matches!(info_1(&mut ctx, &[tab]), Err(BeamError::BadArg)),
                "info/1 of a wrongly-typed table argument"
            );
            assert!(
                matches!(info_2(&mut ctx, &[tab, name_item]), Err(BeamError::BadArg)),
                "info/2 of a wrongly-typed table argument"
            );
            assert!(
                matches!(info_2(&mut ctx, &[tab, one]), Err(BeamError::BadArg)),
                "info/2 of a wrongly-typed table argument"
            );
        }

        // A *live* table with an unknown or non-atom item stays `badarg`.
        let live = new_table(&mut ctx, "mytab");
        assert!(matches!(
            info_2(&mut ctx, &[live, unknown]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            info_2(&mut ctx, &[live, one]),
            Err(BeamError::BadArg)
        ));
    }
    #[test]
    fn update_counter_deposits_and_adds() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 10);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        // `update_counter/3` with a bare increment bumps element `keypos + 1`.
        let incr = ctx.int(5).expect("fits");
        let value = update_counter_3(&mut ctx, &[handle, a, incr]).expect("update_counter");
        assert_eq!(value, ctx.int(15).unwrap());

        // A `{Pos, Incr}` spec addresses element 2 explicitly.
        let two = ctx.int(2).expect("fits");
        let spec = ctx.tuple(&[two, incr]).expect("fits");
        let value = update_counter_3(&mut ctx, &[handle, a, spec]).expect("update_counter");
        assert_eq!(value, ctx.int(20).unwrap());

        // An absent key without a default is `badarg`.
        let b = ctx.atom("b").expect("seeded");
        assert!(matches!(
            update_counter_3(&mut ctx, &[handle, b, incr]),
            Err(BeamError::BadArg)
        ));

        // `update_counter/4` deposits the default (key element replaced).
        let default = ctx.tuple(&[a, ctx.int(0).unwrap()]).expect("fits");
        let value = update_counter_4(&mut ctx, &[handle, b, incr, default]).expect("update_counter/4");
        assert_eq!(value, ctx.int(5).unwrap());
        assert!(is_true(&atoms, member_2(&mut ctx, &[handle, b]).expect("member")));
    }

    #[test]
    fn update_element_replaces_and_reports_presence() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_table(&mut ctx, "mytab");
        let a = ctx.atom("a").expect("seeded");
        let obj_a = pair(&mut ctx, "a", 1);
        insert_2(&mut ctx, &[handle, obj_a]).expect("insert");

        // A list spec replaces element 2; the key position (1) is `badarg`.
        let two = ctx.int(2).expect("fits");
        let nine = ctx.int(9).expect("fits");
        let pair_spec = ctx.tuple(&[two, nine]).expect("fits");
        let list_spec = ctx.list(&[pair_spec]).expect("fits");
        assert!(is_true(
            &atoms,
            update_element_3(&mut ctx, &[handle, a, list_spec]).expect("update_element")
        ));
        let stored = lookup_2(&mut ctx, &[handle, a]).expect("lookup");
        assert_eq!(tuple_element(proper_list(stored).expect("list")[0], 1), nine);

        // A bare `{Pos, Value}` tuple is accepted.
        let seven = ctx.int(7).expect("fits");
        let bare = ctx.tuple(&[two, seven]).expect("fits");
        assert!(is_true(
            &atoms,
            update_element_3(&mut ctx, &[handle, a, bare]).expect("update_element")
        ));

        // An absent key is `false`, whatever the spec says.
        let b = ctx.atom("b").expect("seeded");
        assert!(is_false(
            &atoms,
            update_element_3(&mut ctx, &[handle, b, bare]).expect("update_element")
        ));
        let nonsense = ctx.atom("z").expect("seeded");
        assert!(is_false(
            &atoms,
            update_element_3(&mut ctx, &[handle, b, nonsense]).expect("update_element")
        ));

        // The key position (1) is `badarg` for a present key.
        let one = ctx.int(1).expect("fits");
        let key_spec = ctx.tuple(&[one, nine]).expect("fits");
        assert!(matches!(
            update_element_3(&mut ctx, &[handle, a, key_spec]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn element_pos_and_parse_updates_reject_bad_operands() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let a = ctx.atom("a").expect("seeded");
        let two = ctx.int(2).expect("fits");
        let zero = ctx.int(0).expect("fits");
        let neg = ctx.int(-1).expect("fits");

        assert_eq!(element_pos(two).expect("2 is a valid position"), 2);
        assert_eq!(element_pos(zero).expect("0 parses"), 0);
        assert!(matches!(element_pos(a), Err(BeamError::BadArg)));
        assert!(matches!(element_pos(neg), Err(BeamError::BadArg)));

        // A proper list of 2-tuples parses; a 3-tuple / non-tuple does not.
        let good = ctx.tuple(&[two, a]).expect("fits");
        let list = ctx.list(&[good]).expect("fits");
        assert_eq!(parse_updates(list).expect("parses").len(), 1);
        assert_eq!(parse_updates(good).expect("parses").len(), 1);
        assert!(parse_updates(ctx.list(&[]).expect("fits")).expect("parses").is_empty());
        let three = ctx.int(3).expect("fits");
        let bad_tuple = ctx.tuple(&[two, a, three]).expect("fits");
        assert!(matches!(parse_updates(bad_tuple), Err(BeamError::BadArg)));
        assert!(matches!(parse_updates(a), Err(BeamError::BadArg)));
        let improper = ctx.cons(good, good).expect("fits");
        assert!(matches!(parse_updates(improper), Err(BeamError::BadArg)));
    }

    /// Creates an `ordered_set` table in `ctx` and returns its handle (so the
    /// `select` iteration order is deterministic).
    fn new_ordered_table(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
        let name = ctx.atom(name).expect("seeded");
        let kind = ctx.atom("ordered_set").expect("seeded");
        let opts = ctx.list(&[kind]).expect("fits");
        new_2(ctx, &[name, opts]).expect("ets:new")
    }

    /// The spec `[{{'$1','$2'}, [], [{{'$1','$2'}}]}]` — the whole matched object
    /// per match — built on `ctx`.
    fn whole_object_spec(ctx: &mut BifContext<'_>) -> Eterm {
        let v1 = ctx.atom("$1").expect("seeded");
        let v2 = ctx.atom("$2").expect("seeded");
        let head = ctx.tuple(&[v1, v2]).expect("fits");
        let guards = ctx.list(&[]).expect("fits");
        let projected = ctx.tuple(&[v1, v2]).expect("fits");
        let body_item = ctx.tuple(&[projected]).expect("fits");
        let body = ctx.list(&[body_item]).expect("fits");
        let clause = ctx.tuple(&[head, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    #[test]
    fn select_projects_and_resumes_through_opaque_continuations() {
        let (mut heap, atoms, mut reg) = parts();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut reg);
        let handle = new_ordered_table(&mut ctx, "mytab");
        for (key, value) in [("a", 1), ("b", 2), ("c", 3), ("p", 4)] {
            let obj = pair(&mut ctx, key, value);
            insert_2(&mut ctx, &[handle, obj]).expect("insert");
        }

        // `select/2` projects the whole object for every match, in iteration order.
        let spec = whole_object_spec(&mut ctx);
        let all = select_2(&mut ctx, &[handle, spec]).expect("select/2");
        assert_eq!(proper_list(all).expect("list").len(), 4);

        // `select/3` with a limit yields `{Results, Continuation}`. The continuation
        // is a fresh ref thing in the continuation-id namespace, so it never
        // compares `=:=` to the table handle.
        let two = ctx.int(2).expect("fits");
        let spec = whole_object_spec(&mut ctx);
        let chunk = select_3(&mut ctx, &[handle, spec, two]).expect("select/3");
        let results = tuple_element(chunk, 0);
        let cont = tuple_element(chunk, 1);
        assert_eq!(proper_list(results).expect("list").len(), 2);
        assert!(!beam_term::compare::eq(cont, handle, &atoms));
        assert!(ref_thing_id(cont).expect("continuation ref") >= CONTINUATION_ID_BASE);

        // Resuming draws the second chunk, still with a continuation.
        let chunk = select_1(&mut ctx, &[cont]).expect("select/1");
        let cont = tuple_element(chunk, 1);
        assert_eq!(proper_list(tuple_element(chunk, 0)).expect("list").len(), 2);

        // The traversal finished with no results left, so it renders as the **bare**
        // `'$end_of_table'` atom (the oracle's rule).
        let chunk = select_1(&mut ctx, &[cont]).expect("select/1");
        assert_eq!(atoms.name(atom_val(chunk)), Some(END_OF_TABLE));

        // `limit == 0` is `badarg` (the oracle raises it).
        let zero = ctx.int(0).expect("fits");
        let spec = whole_object_spec(&mut ctx);
        assert!(matches!(
            select_3(&mut ctx, &[handle, spec, zero]),
            Err(BeamError::BadArg)
        ));

        // A non-continuation (here, the table handle) is `badarg`.
        assert!(matches!(
            select_1(&mut ctx, &[handle]),
            Err(BeamError::BadArg)
        ));

        // Deleting the table drops its continuations, so a stale one is `badarg`.
        let spec = whole_object_spec(&mut ctx);
        let chunk = select_3(&mut ctx, &[handle, spec, two]).expect("select/3");
        let stale = tuple_element(chunk, 1);
        assert!(is_true(
            &atoms,
            delete_1(&mut ctx, &[handle]).expect("delete/1")
        ));
        assert!(matches!(
            select_1(&mut ctx, &[stale]),
            Err(BeamError::BadArg)
        ));
    }
}
