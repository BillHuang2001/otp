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

//! Table kinds, access modes and the parsed `ets:new/2` option set.
//!
//! Mirrors the option surface of the legacy `ets:new/2` (`db_new` in
//! `../../erts/emulator/beam/erl_db.c`): the `type` (table kind), `keypos`, the
//! `protection` (access mode), `named_table`, `compressed`,
//! `{write_concurrency, Bool | auto | {debug_hash_fixed_number_of_locks, N}}`
//! (modelled by [`WriteConcurrency`]), `{read_concurrency, Bool}`,
//! `{heir, Pid}`/`{heir, Pid, Data}` and `{decentralized_counters, Bool}`
//! options. Parsing the raw option list happens in `beam_bifs`; this module owns
//! the **parsed** representation ([`TableOptions`]) and its defaults.

use crate::table::{Heir, OwnerId};
use beam_common::Result;
use beam_term::prelude::*;

/// The ETS table kinds — the `type` option of `ets:new/2`.
///
/// The default kind is [`Set`](TableKind::Set), matching `ets:new/2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TableKind {
    /// `set` — at most one object per key (the default).
    #[default]
    Set,
    /// `ordered_set` — at most one object per key, kept in `beam_term` term order.
    OrderedSet,
    /// `bag` — many *distinct* objects per key.
    Bag,
    /// `duplicate_bag` — many objects per key, duplicates allowed.
    DuplicateBag,
}

impl TableKind {
    /// The Erlang atom name of this kind: `"set"`, `"ordered_set"`, `"bag"` or
    /// `"duplicate_bag"`.
    pub fn as_atom_name(&self) -> &'static str {
        match self {
            TableKind::Set => "set",
            TableKind::OrderedSet => "ordered_set",
            TableKind::Bag => "bag",
            TableKind::DuplicateBag => "duplicate_bag",
        }
    }

    /// Parse a table kind from its Erlang atom name; the inverse of
    /// [`as_atom_name`](TableKind::as_atom_name).
    ///
    /// Returns `None` for an unknown name so the `ets:new/2` option parser can
    /// reject it (`badarg`) at the call site.
    pub fn from_atom_name(name: &str) -> Option<TableKind> {
        match name {
            "set" => Some(TableKind::Set),
            "ordered_set" => Some(TableKind::OrderedSet),
            "bag" => Some(TableKind::Bag),
            "duplicate_bag" => Some(TableKind::DuplicateBag),
            _ => None,
        }
    }

    /// Whether the kind stores multiple objects per key (`bag`,
    /// `duplicate_bag`). `false` for `set`/`ordered_set`.
    pub fn is_multi(&self) -> bool {
        matches!(self, TableKind::Bag | TableKind::DuplicateBag)
    }

    /// Whether the kind is ordered (`ordered_set`). `false` otherwise.
    pub fn is_ordered(&self) -> bool {
        matches!(self, TableKind::OrderedSet)
    }
}

/// The index shape backing a **`set`** table.
///
/// The interior shape is not externally pinned (workspace `CONTEXT.md` §2): this
/// is the crate-internal selection knob for the one kind that has two competing
/// interiors ([`crate::HashInterior`], the chained index shared with
/// `bag`/`duplicate_bag`, and [`crate::ChampInterior`], the open-addressing
/// CHAMP). The multi-object kinds ignore it (they always use the chained index)
/// and `ordered_set` ignores it too (its index is chosen by [`OrderedInterior`]).
///
/// Constructor: [`Table::with_set_interior`](crate::Table::with_set_interior).
/// The [`Default`] is the measured winner (see `docs/interior_selection.md` and
/// the crate benchmark `src/interior_bench.rs`); `Test Strategy` in the crate
/// `CONTEXT.md` pins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SetInterior {
    /// The chained-bucket hash index (`HashInterior`): a bucket array of
    /// `(key, Vec<object>)` entries, doubling past a 3/4 load factor.
    Chained,
    /// The CHAMP open-addressing index (`ChampInterior`): compressed bitmap
    /// nodes over the key hash, one inline slot per key.
    #[default]
    Champ,
}

impl SetInterior {
    /// A short name for diagnostics and docs (`"chained"` / `"champ"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            SetInterior::Chained => "chained",
            SetInterior::Champ => "champ",
        }
    }
}

/// The index shape backing an **`ordered_set`** table.
///
/// The interior shape is not externally pinned (workspace `CONTEXT.md` §2): this
/// is the crate-internal selection knob for the one *ordered* kind, which has two
/// competing interiors — the key-sorted B+-tree
/// ([`OrderedSetInterior`](crate::OrderedSetInterior), the default) and the plain
/// sorted `Vec` ([`SortedOrderedInterior`](crate::SortedOrderedInterior)). Every
/// other kind has a single index and ignores it.
///
/// Constructor: [`Table::with_ordered_interior`](crate::Table::with_ordered_interior).
/// The [`Default`] is the measured winner (see the crate benchmark
/// `src/interior_bench.rs`); `Test Strategy` in the crate `CONTEXT.md` pins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OrderedInterior {
    /// The K-ary B+-tree index (`OrderedSetInterior`): arena-backed nodes over
    /// ascending `beam_term` key order, `O(log n)` lookup/insert/delete.
    #[default]
    BPlusTree,
    /// The sorted-`Vec` index (`SortedOrderedInterior`): one sorted entry per key,
    /// `O(log n)` binary-search lookup with `O(n)` insert/delete.
    Sorted,
}

impl OrderedInterior {
    /// A short name for diagnostics and docs (`"bplus_tree"` / `"sorted"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderedInterior::BPlusTree => "bplus_tree",
            OrderedInterior::Sorted => "sorted",
        }
    }
}

/// The table access mode — the `protection` option of `ets:new/2`.
///
/// The default is [`Protected`](Access::Protected), matching `ets:new/2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Access {
    /// `public` — any process may read and write.
    Public,
    /// `protected` — any process may read, only the owner may write (the default).
    #[default]
    Protected,
    /// `private` — only the owner may read and write.
    Private,
}

impl Access {
    /// The Erlang atom name of this access mode: `"public"`, `"protected"` or
    /// `"private"`.
    pub fn as_atom_name(&self) -> &'static str {
        match self {
            Access::Public => "public",
            Access::Protected => "protected",
            Access::Private => "private",
        }
    }

    /// Parse an access mode from its Erlang atom name (`protection` option); the
    /// inverse of [`as_atom_name`](Access::as_atom_name).
    ///
    /// Returns `None` for an unknown name so the `ets:new/2` option parser can
    /// reject it (`badarg`) at the call site.
    pub fn from_atom_name(name: &str) -> Option<Access> {
        match name {
            "public" => Some(Access::Public),
            "protected" => Some(Access::Protected),
            "private" => Some(Access::Private),
            _ => None,
        }
    }
}

/// The table concurrency mode (M1 skeleton).
///
/// Declared for the A.6 configuration surface (`{write_concurrency, _}` /
/// `{read_concurrency, _}`). It is **metadata only** in M1 — no locking or
/// per-reader tables exist yet (see the crate `CONTEXT.md`, Known Issues).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Concurrency {
    /// No concurrency support.
    None,
    /// Concurrent readers only (`read_concurrency`).
    Read,
    /// Concurrent writers only (`write_concurrency`).
    Write,
    /// Concurrent readers and writers (both flags set).
    ReadWrite,
}

impl Concurrency {
    /// The Erlang atom name of this concurrency mode: `"none"`, `"read"`,
    /// `"write"` or `"read_write"`.
    pub fn as_atom_name(&self) -> &'static str {
        match self {
            Concurrency::None => "none",
            Concurrency::Read => "read",
            Concurrency::Write => "write",
            Concurrency::ReadWrite => "read_write",
        }
    }

    /// Parse a concurrency mode from its atom name; the inverse of
    /// [`as_atom_name`](Concurrency::as_atom_name). Returns `None` for an
    /// unknown name.
    pub fn from_atom_name(name: &str) -> Option<Concurrency> {
        match name {
            "none" => Some(Concurrency::None),
            "read" => Some(Concurrency::Read),
            "write" => Some(Concurrency::Write),
            "read_write" => Some(Concurrency::ReadWrite),
            _ => None,
        }
    }
}

/// The parsed `{write_concurrency, …}` mode of `ets:new/2`.
///
/// Models the whole `{write_concurrency, _}` surface the oracle accepts — the
/// boolean forms, the `auto` atom and the hidden
/// `{debug_hash_fixed_number_of_locks, N}` granularity option
/// (`ets_new_2`, `../../erts/emulator/beam/erl_db.c`:2624-2663) — as parsed
/// options, exactly like [`TableKind`]/[`Access`].
///
/// # `ets:info(T, write_concurrency)`
///
/// The oracle's rendering is `table_info` (`erl_db.c`:5492-5506):
///
/// | mode | `ets:info(T, write_concurrency)` |
/// |---|---|
/// | [`Off`](WriteConcurrency::Off) | the atom `false` |
/// | [`On`](WriteConcurrency::On) | the atom `true` |
/// | [`Auto`](WriteConcurrency::Auto) | the atom `auto` (the `DB_FINE_LOCKED_AUTO` flag) |
/// | [`FixedLocks`](WriteConcurrency::FixedLocks)`(N)` | `{debug_hash_fixed_number_of_locks, Nlocks}` |
///
/// so [`as_atom_name`](WriteConcurrency::as_atom_name) returns those three atoms
/// and `None` for [`FixedLocks`](WriteConcurrency::FixedLocks) — the one form
/// that renders as a **tuple**, not an atom. A `private` table never keeps the
/// mode at all (the C's `is_fine_locked && !(status & DB_PRIVATE)` gate); see
/// [`TableOptions::write_concurrency_mode`].
///
/// # The stored lock count is the *requested* one
///
/// [`FixedLocks`](WriteConcurrency::FixedLocks) carries the count **as
/// requested**. The `Nlocks` the oracle reports is that count rounded *down* to a
/// power of two and then clamped *up* to `NLOCKS_WITH_ITEM_COUNTERS` (64), which
/// `db_hash_create`/`db_hash_init` (`../../erts/emulator/beam/erl_db_hash.c`)
/// derives when the table is created — a table-creation detail, not part of this
/// model. So is the `1..=32768` accept range (`DB_WRITE_CONCURRENCY_MIN_LOCKS`/
/// `_MAX_LOCKS`, `erl_db.c`:65-66) and the `ordered_set` rejection of the
/// explicit-granularity form (`erl_db.c`:2727-2732): both belong to the
/// `beam_bifs` option parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WriteConcurrency {
    /// `{write_concurrency, false}`, or the option omitted — the default. The
    /// table is not fine-locked and reports the atom `false`.
    #[default]
    Off,
    /// `{write_concurrency, true}` — fine-locked with the implementation's
    /// default lock count. Reports the atom `true`.
    On,
    /// `{write_concurrency, auto}` — fine-locked with an automatically sized lock
    /// set. Reports the atom `auto`.
    Auto,
    /// `{write_concurrency, {debug_hash_fixed_number_of_locks, N}}` — fine-locked
    /// with an explicit lock granularity of the requested `N` locks. Reports the
    /// tuple `{debug_hash_fixed_number_of_locks, Nlocks}` (the normalized count).
    FixedLocks(u32),
}

impl WriteConcurrency {
    /// Whether write concurrency is enabled at all — `false` only for
    /// [`Off`](WriteConcurrency::Off).
    ///
    /// This is the C's `is_fine_locked` flag *before* the `DB_PRIVATE` gate:
    /// `true` for every form that asks for concurrent writers
    /// (`{write_concurrency, true}` / `auto` / `{debug_hash_fixed_number_of_locks,
    /// _}`).
    pub fn is_enabled(&self) -> bool {
        !matches!(self, WriteConcurrency::Off)
    }

    /// The requested lock count for
    /// [`FixedLocks`](WriteConcurrency::FixedLocks), `None` for every other mode.
    ///
    /// The count is the one the option named, un-normalized (see the type's
    /// rustdoc).
    pub fn fixed_locks(&self) -> Option<u32> {
        match self {
            WriteConcurrency::FixedLocks(locks) => Some(*locks),
            _ => None,
        }
    }

    /// The Erlang **atom** `ets:info(T, write_concurrency)` reports for this mode:
    /// `"false"`, `"true"` or `"auto"` — and `None` for
    /// [`FixedLocks`](WriteConcurrency::FixedLocks), which the oracle renders as
    /// the tuple `{debug_hash_fixed_number_of_locks, Nlocks}`.
    pub fn as_atom_name(&self) -> Option<&'static str> {
        match self {
            WriteConcurrency::Off => Some("false"),
            WriteConcurrency::On => Some("true"),
            WriteConcurrency::Auto => Some("auto"),
            WriteConcurrency::FixedLocks(_) => None,
        }
    }

    /// Parse a write-concurrency mode from the atom the option named; the
    /// inverse of [`as_atom_name`](WriteConcurrency::as_atom_name) for the three
    /// atom forms.
    ///
    /// Returns `None` for anything else, so the `ets:new/2` option parser can
    /// reject it (`badarg`) at the call site. Note that this covers only the atom
    /// forms: the `{debug_hash_fixed_number_of_locks, N}` value is a tuple, so it
    /// is built by the parser (a [`FixedLocks`](WriteConcurrency::FixedLocks))
    /// rather than parsed here.
    pub fn from_atom_name(name: &str) -> Option<WriteConcurrency> {
        match name {
            "false" => Some(WriteConcurrency::Off),
            "true" => Some(WriteConcurrency::On),
            "auto" => Some(WriteConcurrency::Auto),
            _ => None,
        }
    }
}

/// The parsed `ets:new/2` option set.
///
/// Built with the fluent builder ([`TableOptions::new`] plus the `set_*`
/// methods) and read through the getters. Defaults: [`TableKind::Set`],
/// `keypos` 1, [`Access::Protected`], not named, no name, not compressed, no
/// concurrency, no heir and no explicit `decentralized_counters` option.
#[derive(Debug, Clone)]
pub struct TableOptions {
    /// The table kind.
    kind: TableKind,
    /// The 1-based key position.
    keypos: usize,
    /// The access mode.
    access: Access,
    /// Whether `named_table` was requested.
    named: bool,
    /// The registered name, if one was given. Recorded for every table (the
    /// oracle's `ets:info/2` `name` always returns it), independent of the
    /// `named_table` flag.
    name: Option<Atom>,
    /// Whether `compressed` was requested.
    compressed: bool,
    /// The requested `{write_concurrency, …}` mode. The **effective** value is
    /// [`TableOptions::write_concurrency_mode`] (a `private` table forces it to
    /// [`WriteConcurrency::Off`]), whose `Off`/`On` projection is
    /// [`TableOptions::write_concurrency`].
    write_concurrency: WriteConcurrency,
    /// Whether `read_concurrency` was requested. The **effective** value is
    /// [`TableOptions::read_concurrency`] (a `private` table forces it off).
    read_concurrency: bool,
    /// The **creation-time** heir (`{heir, Pid}` / `{heir, Pid, Data}`), if one was
    /// given. The value is adopted by [`Table::build`](crate::Table) as the table's
    /// initial heir, which **deep-copies** its gift data into table-owned storage
    /// (so the stored term does not alias the caller's heap); `ets:setopts(T,
    /// {heir, …})` replaces it later. Its [`data`](Heir::data) records the option's
    /// arity (`Some` for arity 3, `None` for arity 2) and holds the caller's raw
    /// term here — the copy happens when the table is built, not in this builder.
    heir: Option<Heir>,
    /// The explicit `{decentralized_counters, Bool}` creation option, if given.
    /// `None` means "not requested", i.e. the kind's default (see
    /// [`TableOptions::decentralized_counters`]).
    decentralized_counters: Option<bool>,
}

impl TableOptions {
    /// A fresh option set with the `ets:new/2` defaults.
    pub fn new() -> TableOptions {
        TableOptions {
            kind: TableKind::default(),
            keypos: 1,
            access: Access::default(),
            named: false,
            name: None,
            compressed: false,
            write_concurrency: WriteConcurrency::Off,
            read_concurrency: false,
            heir: None,
            decentralized_counters: None,
        }
    }

    /// Set the table kind.
    pub fn set_kind(&mut self, kind: TableKind) -> &mut Self {
        self.kind = kind;
        self
    }

    /// Set the 1-based `keypos`. Rejects `0` with `badarg`.
    pub fn set_keypos(&mut self, keypos: usize) -> Result<&mut Self> {
        if keypos == 0 {
            return Err(crate::error::badarg());
        }
        self.keypos = keypos;
        Ok(self)
    }

    /// Set the access mode (`protection`).
    pub fn set_access(&mut self, access: Access) -> &mut Self {
        self.access = access;
        self
    }

    /// Set whether the table is named (`named_table`).
    pub fn set_named(&mut self, named: bool) -> &mut Self {
        self.named = named;
        self
    }

    /// Record the registered table name.
    ///
    /// This records the name only; it does **not** by itself mark the table
    /// `named_table` (that is [`set_named`](TableOptions::set_named)). The two
    /// are independent, exactly as in the oracle: `ets:new(myanon, [])` reports
    /// `name => myanon` but `named_table => false`, and only the `named_table`
    /// option makes the name resolve to the table.
    pub fn set_name(&mut self, name: Atom) -> &mut Self {
        self.name = Some(name);
        self
    }

    /// Set whether stored terms are compressed (`compressed`).
    pub fn set_compressed(&mut self, compressed: bool) -> &mut Self {
        self.compressed = compressed;
        self
    }

    /// Set whether concurrent writers are allowed (`{write_concurrency, Bool}`).
    ///
    /// The boolean form only: `false` selects [`WriteConcurrency::Off`] and `true`
    /// [`WriteConcurrency::On`]. The richer `auto` /
    /// `{debug_hash_fixed_number_of_locks, N}` forms go through
    /// [`set_write_concurrency_mode`](TableOptions::set_write_concurrency_mode).
    pub fn set_write_concurrency(&mut self, write: bool) -> &mut Self {
        self.write_concurrency = if write {
            WriteConcurrency::On
        } else {
            WriteConcurrency::Off
        };
        self
    }

    /// Set the whole parsed `{write_concurrency, …}` mode.
    ///
    /// Installs the mode **as requested**: `auto` and
    /// `{debug_hash_fixed_number_of_locks, N}` are stored verbatim, and a `private`
    /// table only has them normalized away when they are read
    /// ([`write_concurrency_mode`](TableOptions::write_concurrency_mode)), so the
    /// builder may be told the access mode first or last.
    pub fn set_write_concurrency_mode(&mut self, mode: WriteConcurrency) -> &mut Self {
        self.write_concurrency = mode;
        self
    }
    /// Set whether concurrent readers are allowed (`read_concurrency`).
    pub fn set_read_concurrency(&mut self, read: bool) -> &mut Self {
        self.read_concurrency = read;
        self
    }

    /// Install the **creation-time** heir (`{heir, Pid}` / `{heir, Pid, Data}` /
    /// `{heir, none}`).
    ///
    /// `Some(heir)` records the heir a freshly built table starts with —
    /// [`Table::build`](crate::Table) adopts it — and `None` clears it (what
    /// `{heir, none}` means at creation). Infallible: unlike the raw option term
    /// there is no malformed value to reject here, so the `badarg` for a
    /// non-pid/`none` heir belongs to the `beam_bifs` option parser.
    ///
    /// The option's **arity** is carried in [`Heir::data`]: `Some(term)` is
    /// `{heir, Pid, Data}` (arity 3, whose `term` the oracle delivers verbatim —
    /// `undefined` included) and `None` is `{heir, Pid}` (arity 2, no gift data, so
    /// the oracle sends **no** `{'ETS-TRANSFER', …}` message on owner death). That
    /// distinction is the parser's to get right, just like the `none`/pid check.
    ///
    /// This builder records the gift data **as given** (it has no store of its own);
    /// [`Table::build`](crate::Table) then **deep-copies** it into the table's
    /// table-owned storage, exactly as [`Table::install_heir`](crate::Table::install_heir)
    /// does for a later `setopts/2`, so the resulting creation-time heir does not
    /// alias the caller's heap and its copy is reclaimed when the heir is replaced
    /// or cleared.
    pub fn set_heir(&mut self, heir: Option<Heir>) -> &mut Self {
        self.heir = heir;
        self
    }

    /// Set the explicit `{decentralized_counters, Bool}` creation option.
    ///
    /// `Some(value)` records the explicit option — which **wins** over the kind's
    /// default whatever its value (the C `decentralized_counters_option`
    /// override) — and `None` clears it, restoring the default. Infallible: the
    /// `badarg` for a non-boolean `{decentralized_counters, _}` belongs to the
    /// `beam_bifs` option parser.
    pub fn set_decentralized_counters(&mut self, explicit: Option<bool>) -> &mut Self {
        self.decentralized_counters = explicit;
        self
    }

    /// The configured table kind.
    pub fn kind(&self) -> TableKind {
        self.kind
    }

    /// The configured 1-based key position.
    pub fn keypos(&self) -> usize {
        self.keypos
    }

    /// The configured access mode.
    pub fn access(&self) -> Access {
        self.access
    }

    /// Whether the table is named.
    pub fn is_named(&self) -> bool {
        self.named
    }

    /// The configured table name, if any.
    pub fn name(&self) -> Option<Atom> {
        self.name
    }

    /// Whether compression was requested.
    pub fn is_compressed(&self) -> bool {
        self.compressed
    }

    /// Whether `write_concurrency` is **in effect** for the table — the `On`/`Off`
    /// projection of the effective [`write_concurrency_mode`](Self::write_concurrency_mode).
    ///
    /// A `private` table never gets concurrent writers: the oracle only sets the
    /// fine-locked flag when the table is not `DB_PRIVATE`
    /// (`if (is_fine_locked && !(status & DB_PRIVATE))`,
    /// `../../erts/emulator/beam/erl_db.c`), so
    /// `ets:new(t, [private, {write_concurrency, true}])` reports `false` (verified
    /// against the system `erl`, OTP 29 / erts-17.1). True for **every** enabled
    /// mode (`true`, `auto`, `{debug_hash_fixed_number_of_locks, _}`) — the flag
    /// says the table is fine-locked, not which form was requested (that is what
    /// [`write_concurrency_mode`](Self::write_concurrency_mode) is for). The
    /// normalization lives in this getter — not in
    /// [`Table::info`](crate::Table::info) — so the option set and every reader of
    /// the flag agree, and it is **order-independent**: the builder may be told the
    /// access mode or the flag first.
    pub fn write_concurrency(&self) -> bool {
        self.write_concurrency_mode().is_enabled()
    }

    /// The **effective** [`WriteConcurrency`] mode: the mode as requested, or
    /// [`WriteConcurrency::Off`] when the table is `private`.
    ///
    /// `private` is the one access mode the oracle treats specially: it forces the
    /// fine-locked flag off (`if (is_fine_locked && !(status & DB_PRIVATE))`,
    /// `erl_db.c`), so a private table reports `write_concurrency => false` however
    /// the option asked — the atom `auto`/the
    /// `{debug_hash_fixed_number_of_locks, _}` tuple included. Like
    /// [`write_concurrency`](Self::write_concurrency) the normalization is applied
    /// on read, so it is order-independent.
    pub fn write_concurrency_mode(&self) -> WriteConcurrency {
        if self.is_private() {
            WriteConcurrency::Off
        } else {
            self.write_concurrency
        }
    }
    /// Whether `read_concurrency` is **in effect** for the table. Normalized like
    /// [`write_concurrency`](TableOptions::write_concurrency): a `private` table
    /// reports `false` even when the flag was requested.
    pub fn read_concurrency(&self) -> bool {
        self.read_concurrency && !self.is_private()
    }

    /// Whether this option set describes a `private` table.
    ///
    /// `private` is the one access mode the oracle treats specially: it forces the
    /// concurrency flags off. `compressed` is **not** affected (a private
    /// compressed table reports `compressed => true`), so it is deliberately left
    /// out of the normalization.
    fn is_private(&self) -> bool {
        self.access == Access::Private
    }

    /// The creation-time heir, if one was requested. Its [`data`](Heir::data) is
    /// `Some(term)` for `{heir, Pid, Data}` and `None` for `{heir, Pid}`.
    ///
    /// This is the **raw** option: a non-immediate `data` is the caller's term,
    /// recorded as given. [`Table::build`](crate::Table) deep-copies it into
    /// table-owned storage when the table is constructed.
    pub fn heir(&self) -> Option<Heir> {
        self.heir
    }

    /// The **explicit** `{decentralized_counters, Bool}` option exactly as given,
    /// or `None` when the option was not requested.
    ///
    /// This is the raw parsed option; [`decentralized_counters`](Self::decentralized_counters)
    /// is the value the oracle actually reports.
    pub fn decentralized_counters_option(&self) -> Option<bool> {
        self.decentralized_counters
    }

    /// The `decentralized_counters` value the oracle reports for a table built
    /// from this option set — the value `ets:info(T, decentralized_counters)`
    /// returns.
    ///
    /// Measured against the system `erl` (OTP 29.1.1 / erts-17.1) and matching the
    /// legacy C `db_new` (`../../erts/emulator/beam/erl_db.c`), the observable
    /// value is `DB_FINE_LOCKED && is_decentralized_counters`, where
    /// `is_decentralized_counters` is the **explicit** option when given and
    /// otherwise derived by the C's option loop: `true` for an `ordered_set` (the
    /// `am_ordered_set` arm), `true` for `{write_concurrency, auto}` and for
    /// `{write_concurrency, {debug_hash_fixed_number_of_locks, _}}` (both arms set
    /// it unconditionally), and `false` for a plain `{write_concurrency, true}`
    /// unless the table is an `ordered_set` (`if (!(status & DB_ORDERED_SET))
    /// is_decentralized_counters = false;`). Concretely:
    ///
    /// * `ordered_set` with `{write_concurrency, true}` ⇒ `true` (the CATREE
    ///   path), and an explicit `{decentralized_counters, false}` overrides it
    ///   back to `false`;
    /// * `set`/`bag`/`duplicate_bag` with `{write_concurrency, true}` ⇒ `false`,
    ///   but `{decentralized_counters, true}` **together with**
    ///   `{write_concurrency, true}` ⇒ `true`;
    /// * **every** kind with `{write_concurrency, auto}` or
    ///   `{write_concurrency, {debug_hash_fixed_number_of_locks, _}}` ⇒ `true`,
    ///   unless an explicit `{decentralized_counters, false}` says otherwise;
    /// * the explicit option **alone** (no effective `write_concurrency`) ⇒
    ///   `false` — the fine-locked flag gates it, and a `private` table never gets
    ///   that flag (hence the normalized
    ///   [`write_concurrency`](TableOptions::write_concurrency) here), so a private
    ///   table reports `false` even for the `auto`/fixed-locks forms.
    pub fn decentralized_counters(&self) -> bool {
        // `DB_FINE_LOCKED` (the normalized concurrency flag) gates the counters
        // flag, which is the explicit option when given and otherwise the kind's
        // default: `true` for `ordered_set`, and `true` for the `auto` and
        // fixed-lock forms whatever the kind (they ask for item counters).
        let counters_flag = self.decentralized_counters.unwrap_or_else(|| {
            self.kind.is_ordered()
                || matches!(
                    self.write_concurrency_mode(),
                    WriteConcurrency::Auto | WriteConcurrency::FixedLocks(_)
                )
        });
        self.write_concurrency() && counters_flag
    }
    /// The concurrency mode in effect, derived from the normalized
    /// [`read_concurrency`](TableOptions::read_concurrency) /
    /// [`write_concurrency`](TableOptions::write_concurrency) flags (both set ⇒
    /// [`Concurrency::ReadWrite`]; a `private` table is always
    /// [`Concurrency::None`]).
    pub fn concurrency(&self) -> Concurrency {
        match (self.read_concurrency(), self.write_concurrency()) {
            (true, true) => Concurrency::ReadWrite,
            (true, false) => Concurrency::Read,
            (false, true) => Concurrency::Write,
            (false, false) => Concurrency::None,
        }
    }
}

impl Default for TableOptions {
    fn default() -> TableOptions {
        TableOptions::new()
    }
}

/// Whether `requester` is the table owner.
///
/// The owner comparison is exactly `requester == owner`, matching the legacy
/// `p->common.id != tb->common.owner` check in `handle_lacking_permission`
/// (`../../erts/emulator/beam/erl_db.c`). Consequently `None == Some(x)` is
/// `false` (an anonymous requester never owns a table), while an ownerless table
/// accessed by an anonymous requester (`None == None`) is permitted.
fn is_owner(owner: Option<OwnerId>, requester: Option<OwnerId>) -> bool {
    requester == owner
}

/// Whether `requester` may read a table with the given access mode.
///
/// Reproduces the access model of `db_get_table_aux` / `handle_lacking_permission`
/// in `../../erts/emulator/beam/erl_db.c` (via the `DB_READ` status mask
/// `DB_PROTECTED | DB_PUBLIC`):
/// * `public` — anyone may read.
/// * `protected` — anyone may read.
/// * `private` — only the owner may read.
pub fn can_read(access: Access, owner: Option<OwnerId>, requester: Option<OwnerId>) -> bool {
    match access {
        Access::Public | Access::Protected => true,
        Access::Private => is_owner(owner, requester),
    }
}

/// Whether `requester` may write a table with the given access mode.
///
/// Reproduces the access model of `db_get_table_aux` / `handle_lacking_permission`
/// in `../../erts/emulator/beam/erl_db.c` (via the `DB_WRITE` status mask
/// `DB_PUBLIC`):
/// * `public` — anyone may write.
/// * `protected` — only the owner may write.
/// * `private` — only the owner may write.
pub fn can_write(access: Access, owner: Option<OwnerId>, requester: Option<OwnerId>) -> bool {
    match access {
        Access::Public => true,
        Access::Protected | Access::Private => is_owner(owner, requester),
    }
}

#[cfg(test)]
mod tests;
