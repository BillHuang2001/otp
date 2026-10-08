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
//! `{write_concurrency, Bool}`, `{read_concurrency, Bool}`,
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
    /// Whether `write_concurrency` was requested. The **effective** value is
    /// [`TableOptions::write_concurrency`] (a `private` table forces it off).
    write_concurrency: bool,
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
            write_concurrency: false,
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

    /// Set whether concurrent writers are allowed (`write_concurrency`).
    pub fn set_write_concurrency(&mut self, write: bool) -> &mut Self {
        self.write_concurrency = write;
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

    /// Whether `write_concurrency` is **in effect** for the table.
    ///
    /// A `private` table never gets concurrent writers: the oracle only sets the
    /// fine-locked flag when the table is not `DB_PRIVATE`
    /// (`if (is_fine_locked && !(status & DB_PRIVATE))`,
    /// `../../erts/emulator/beam/erl_db.c`), so
    /// `ets:new(t, [private, {write_concurrency, true}])` reports `false` (verified
    /// against the system `erl`, OTP 29 / erts-17.1). The normalization lives in
    /// this getter — not in [`Table::info`](crate::Table::info) — so the option set
    /// and every reader of the flag agree, and it is **order-independent**: the
    /// builder may be told the access mode or the flag first.
    pub fn write_concurrency(&self) -> bool {
        self.write_concurrency && !self.is_private()
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
    /// otherwise `true` only for an `ordered_set` (the `am_ordered_set` list
    /// element sets it). Concretely:
    ///
    /// * `ordered_set` with `{write_concurrency, true}` ⇒ `true` (the CATREE
    ///   path), and an explicit `{decentralized_counters, false}` overrides it
    ///   back to `false`;
    /// * `set`/`bag`/`duplicate_bag` with `{write_concurrency, true}` ⇒ `false`,
    ///   but `{decentralized_counters, true}` **together with**
    ///   `{write_concurrency, true}` ⇒ `true`;
    /// * the explicit option **alone** (no effective `write_concurrency`) ⇒
    ///   `false` — the fine-locked flag gates it, and a `private` table never gets
    ///   that flag (hence the normalized
    ///   [`write_concurrency`](TableOptions::write_concurrency) here).
    pub fn decentralized_counters(&self) -> bool {
        // `DB_FINE_LOCKED` (the normalized concurrency flag) gates the counters
        // flag, which is the explicit option when given and the kind's default
        // (`true` only for `ordered_set`) otherwise.
        let counters_flag = self
            .decentralized_counters
            .unwrap_or(self.kind.is_ordered());
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
mod tests {
    use super::*;
    use beam_common::BeamError;

    #[test]
    fn table_kind_atom_names_and_predicates() {
        assert_eq!(TableKind::Set.as_atom_name(), "set");
        assert_eq!(TableKind::OrderedSet.as_atom_name(), "ordered_set");
        assert_eq!(TableKind::Bag.as_atom_name(), "bag");
        assert_eq!(TableKind::DuplicateBag.as_atom_name(), "duplicate_bag");

        assert!(!TableKind::Set.is_multi());
        assert!(!TableKind::OrderedSet.is_multi());
        assert!(TableKind::Bag.is_multi());
        assert!(TableKind::DuplicateBag.is_multi());

        assert!(!TableKind::Set.is_ordered());
        assert!(TableKind::OrderedSet.is_ordered());
        assert!(!TableKind::Bag.is_ordered());
        assert!(!TableKind::DuplicateBag.is_ordered());
    }

    #[test]
    fn table_kind_default_is_set() {
        assert_eq!(TableKind::default(), TableKind::Set);
    }

    #[test]
    fn access_atom_names_and_default() {
        assert_eq!(Access::Public.as_atom_name(), "public");
        assert_eq!(Access::Protected.as_atom_name(), "protected");
        assert_eq!(Access::Private.as_atom_name(), "private");
        assert_eq!(Access::default(), Access::Protected);
    }

    #[test]
    fn defaults_match_ets_new() {
        let opts = TableOptions::new();
        assert_eq!(opts.kind(), TableKind::Set);
        assert_eq!(opts.keypos(), 1);
        assert_eq!(opts.access(), Access::Protected);
        assert!(!opts.is_named());
        assert_eq!(opts.name(), None);
        assert!(!opts.is_compressed());
        assert!(!opts.write_concurrency());
        assert!(!opts.read_concurrency());
        assert_eq!(opts.heir(), None);
        assert_eq!(opts.decentralized_counters_option(), None);
        assert!(!opts.decentralized_counters());
    }

    #[test]
    fn default_equals_new() {
        let from_new = TableOptions::new();
        let from_default = TableOptions::default();
        assert_eq!(from_new.kind(), from_default.kind());
        assert_eq!(from_new.keypos(), from_default.keypos());
        assert_eq!(from_new.access(), from_default.access());
        assert_eq!(from_new.is_named(), from_default.is_named());
        assert_eq!(from_new.name(), from_default.name());
        assert_eq!(from_new.is_compressed(), from_default.is_compressed());
    }

    #[test]
    fn set_keypos_rejects_zero_but_accepts_positives() {
        let mut opts = TableOptions::new();
        assert!(matches!(
            opts.set_keypos(0),
            Err(BeamError::BadArg)
        ));
        // A rejected set must not have mutated the option set.
        assert_eq!(opts.keypos(), 1);

        assert!(opts.set_keypos(3).is_ok());
        assert_eq!(opts.keypos(), 3);
    }

    #[test]
    fn builder_methods_update_and_return_self() {
        let mut opts = TableOptions::new();
        opts.set_kind(TableKind::Bag)
            .set_access(Access::Public)
            .set_compressed(true)
            .set_write_concurrency(true)
            .set_read_concurrency(true)
            .set_named(true);

        assert_eq!(opts.kind(), TableKind::Bag);
        assert_eq!(opts.access(), Access::Public);
        assert!(opts.is_compressed());
        assert!(opts.write_concurrency());
        assert!(opts.read_concurrency());
        assert!(opts.is_named());
    }

    /// The measured `ets:info(T, decentralized_counters)` rule, row by row.
    ///
    /// Every row is a live-oracle observation (system `erl`, OTP 29.1.1 /
    /// erts-17.1) of `ets:new/2` followed by `ets:info(T, decentralized_counters)`
    /// and reproduces the C `db_new`: the observable value is
    /// `DB_FINE_LOCKED && is_decentralized_counters`, the explicit option
    /// overrides the kind's default (which is `true` only for `ordered_set`), and
    /// `DB_FINE_LOCKED` needs an effective `write_concurrency`.
    #[test]
    fn decentralized_counters_follows_the_measured_oracle_rule() {
        // (kind, access, write_concurrency, explicit option, oracle value)
        let rows: &[(TableKind, Access, bool, Option<bool>, bool)] = &[
            // (a) option omitted: only `ordered_set` + effective `write_concurrency`.
            (TableKind::Set, Access::Protected, false, None, false),
            (TableKind::OrderedSet, Access::Protected, false, None, false),
            (TableKind::Bag, Access::Protected, false, None, false),
            (TableKind::DuplicateBag, Access::Protected, false, None, false),
            (TableKind::Set, Access::Protected, true, None, false),
            (TableKind::OrderedSet, Access::Protected, true, None, true),
            (TableKind::Bag, Access::Protected, true, None, false),
            (TableKind::DuplicateBag, Access::Protected, true, None, false),
            // (b) an explicit option alone is gated on the fine-locked flag.
            (TableKind::Set, Access::Protected, false, Some(true), false),
            (TableKind::OrderedSet, Access::Protected, false, Some(true), false),
            (TableKind::Bag, Access::Protected, false, Some(true), false),
            // (c) the explicit option wins for a non-`ordered_set` kind.
            (TableKind::Set, Access::Protected, true, Some(true), true),
            (TableKind::Bag, Access::Protected, true, Some(true), true),
            (TableKind::DuplicateBag, Access::Protected, true, Some(true), true),
            // ... and it overrides `ordered_set`'s default both ways.
            (TableKind::OrderedSet, Access::Protected, true, Some(true), true),
            (TableKind::OrderedSet, Access::Protected, true, Some(false), false),
            (TableKind::OrderedSet, Access::Protected, false, Some(false), false),
            (TableKind::Set, Access::Protected, true, Some(false), false),
            // A `private` table never gets the fine-locked flag.
            (TableKind::OrderedSet, Access::Private, true, None, false),
            (TableKind::Set, Access::Private, true, Some(true), false),
            // `read_concurrency` does not affect it.
            (TableKind::OrderedSet, Access::Public, true, None, true),
        ];

        for &(kind, access, write_concurrency, explicit, expected) in rows {
            let mut opts = TableOptions::new();
            opts.set_kind(kind)
                .set_access(access)
                .set_write_concurrency(write_concurrency)
                .set_read_concurrency(write_concurrency)
                .set_decentralized_counters(explicit);
            assert_eq!(
                opts.decentralized_counters(),
                expected,
                "{kind:?} {access:?} write_concurrency={write_concurrency} \
                 explicit={explicit:?}"
            );
            assert_eq!(opts.decentralized_counters_option(), explicit);
        }
    }

    /// Clearing the explicit option restores the kind's default; `TableOptions`
    /// never hides the raw option.
    #[test]
    fn decentralized_counters_option_round_trips_and_resets() {
        let mut opts = TableOptions::new();
        opts.set_kind(TableKind::Set).set_write_concurrency(true);
        assert!(!opts.decentralized_counters());

        opts.set_decentralized_counters(Some(true));
        assert_eq!(opts.decentralized_counters_option(), Some(true));
        assert!(opts.decentralized_counters());

        opts.set_decentralized_counters(None);
        assert_eq!(opts.decentralized_counters_option(), None);
        assert!(!opts.decentralized_counters());

        // `{decentralized_counters,false}` on an `ordered_set` sticks.
        let mut ordered = TableOptions::new();
        ordered
            .set_kind(TableKind::OrderedSet)
            .set_write_concurrency(true)
            .set_decentralized_counters(Some(false));
        assert!(!ordered.decentralized_counters());
    }

    /// The creation-time heir is stored on the options **as given** (gift data
    /// included — the copy into table storage happens later, in
    /// [`Table::build`](crate::Table)) and can be cleared, exactly like
    /// `{heir, Pid, Data}` / `{heir, none}` at creation. The arity-2
    /// `{heir, Pid}` form is the `data: None` heir.
    #[test]
    fn creation_time_heir_is_stored_and_cleared() {
        let mut opts = TableOptions::new();
        assert_eq!(opts.heir(), None);

        let heir = Heir {
            owner: OwnerId::new(4),
            data: Some(make_small(11)),
        };
        opts.set_heir(Some(heir));
        assert_eq!(opts.heir(), Some(heir));
        assert_eq!(opts.heir().and_then(|h| h.data), Some(make_small(11)));

        // The arity-2 form: an heir with no gift data at all.
        let arity_two = Heir {
            owner: OwnerId::new(4),
            data: None,
        };
        opts.set_heir(Some(arity_two));
        assert_eq!(opts.heir(), Some(arity_two));
        assert_eq!(opts.heir().and_then(|h| h.data), None);

        opts.set_heir(None);
        assert_eq!(opts.heir(), None);
    }

    #[test]
    fn chained_keypos_returns_self() {
        let mut opts = TableOptions::new();
        let returned = opts.set_keypos(2).unwrap();
        assert_eq!(returned.keypos(), 2);
        assert_eq!(opts.keypos(), 2);
    }

    #[test]
    fn set_name_does_not_imply_named() {
        // `name` and `named_table` are independent, as in the oracle:
        // `ets:info(ets:new(n, []), named_table) =:= false` while `name` is `n`.
        let mut opts = TableOptions::new();
        assert!(!opts.is_named());

        let name = Atom::from_raw(7);
        opts.set_name(name);
        assert!(!opts.is_named());
        assert_eq!(opts.name(), Some(name));

        // The `named_table` option is what sets the flag (`set_named`).
        opts.set_named(true);
        assert!(opts.is_named());
        assert_eq!(opts.name(), Some(name));
    }

    #[test]
    fn set_named_false_does_not_clear_name() {
        // `named_table` and the registered name are separate fields; clearing the
        // flag leaves the name intact (the name is only set via `set_name`).
        let mut opts = TableOptions::new();
        opts.set_name(Atom::from_raw(3));
        opts.set_named(false);
        assert!(!opts.is_named());
        assert_eq!(opts.name(), Some(Atom::from_raw(3)));
    }

    #[test]
    fn table_kind_atom_name_round_trip() {
        for kind in [
            TableKind::Set,
            TableKind::OrderedSet,
            TableKind::Bag,
            TableKind::DuplicateBag,
        ] {
            assert_eq!(TableKind::from_atom_name(kind.as_atom_name()), Some(kind));
        }
        assert_eq!(TableKind::from_atom_name("nope"), None);
        assert_eq!(TableKind::from_atom_name(""), None);
    }

    #[test]
    fn access_atom_name_round_trip() {
        for access in [Access::Public, Access::Protected, Access::Private] {
            assert_eq!(Access::from_atom_name(access.as_atom_name()), Some(access));
        }
        assert_eq!(Access::from_atom_name("nope"), None);
        assert_eq!(Access::from_atom_name(""), None);
    }

    #[test]
    fn concurrency_atom_name_round_trip() {
        assert_eq!(Concurrency::None.as_atom_name(), "none");
        assert_eq!(Concurrency::Read.as_atom_name(), "read");
        assert_eq!(Concurrency::Write.as_atom_name(), "write");
        assert_eq!(Concurrency::ReadWrite.as_atom_name(), "read_write");

        for concurrency in [
            Concurrency::None,
            Concurrency::Read,
            Concurrency::Write,
            Concurrency::ReadWrite,
        ] {
            assert_eq!(
                Concurrency::from_atom_name(concurrency.as_atom_name()),
                Some(concurrency)
            );
        }
        assert_eq!(Concurrency::from_atom_name("nope"), None);
        assert_eq!(Concurrency::from_atom_name(""), None);
    }

    #[test]
    fn options_concurrency_derives_from_flags() {
        let mut opts = TableOptions::new();
        assert_eq!(opts.concurrency(), Concurrency::None);

        opts.set_read_concurrency(true);
        assert_eq!(opts.concurrency(), Concurrency::Read);

        opts.set_write_concurrency(true);
        assert_eq!(opts.concurrency(), Concurrency::ReadWrite);

        opts.set_read_concurrency(false);
        assert_eq!(opts.concurrency(), Concurrency::Write);

        opts.set_write_concurrency(false);
        assert_eq!(opts.concurrency(), Concurrency::None);
    }

    /// A `private` table reports both concurrency flags as `false` however they
    /// were requested — and whatever the order, since the rule is applied when the
    /// flags are read. Verified against the system `erl` (OTP 29 / erts-17.1):
    /// `ets:info(ets:new(t, [private, {write_concurrency,true},
    /// {read_concurrency,true}]), write_concurrency) =:= false`.
    #[test]
    fn private_tables_force_the_concurrency_flags_off() {
        for access in [Access::Public, Access::Protected] {
            let mut opts = TableOptions::new();
            opts.set_access(access)
                .set_write_concurrency(true)
                .set_read_concurrency(true);
            assert!(opts.write_concurrency(), "{access:?} keeps write_concurrency");
            assert!(opts.read_concurrency(), "{access:?} keeps read_concurrency");
            assert_eq!(opts.concurrency(), Concurrency::ReadWrite);
        }

        // The flag may be set before or after the access mode: both orders agree.
        let mut flag_first = TableOptions::new();
        flag_first
            .set_write_concurrency(true)
            .set_read_concurrency(true)
            .set_access(Access::Private);
        let mut access_first = TableOptions::new();
        access_first
            .set_access(Access::Private)
            .set_write_concurrency(true)
            .set_read_concurrency(true);
        for opts in [&flag_first, &access_first] {
            assert!(!opts.write_concurrency());
            assert!(!opts.read_concurrency());
            assert_eq!(opts.concurrency(), Concurrency::None);
        }

        // `compressed` is **not** forced off by `private` (the oracle reports
        // `compressed => true` for a private compressed table).
        let mut compressed = TableOptions::new();
        compressed.set_access(Access::Private).set_compressed(true);
        assert!(compressed.is_compressed());
        assert!(!compressed.write_concurrency());
    }
    #[test]
    fn can_read_matrix() {
        let a = OwnerId::new(1);
        let b = OwnerId::new(2);

        // Public: anyone may read, regardless of owner/requester.
        for owner in [None, Some(a)] {
            for requester in [None, Some(a), Some(b)] {
                assert!(
                    can_read(Access::Public, owner, requester),
                    "public read owner={owner:?} requester={requester:?}"
                );
            }
        }

        // Protected: anyone may read, regardless of owner/requester.
        for owner in [None, Some(a)] {
            for requester in [None, Some(a), Some(b)] {
                assert!(
                    can_read(Access::Protected, owner, requester),
                    "protected read owner={owner:?} requester={requester:?}"
                );
            }
        }

        // Private: only an exact owner match may read.
        assert!(can_read(Access::Private, None, None)); // None == None
        assert!(!can_read(Access::Private, None, Some(a)));
        assert!(!can_read(Access::Private, Some(a), None));
        assert!(can_read(Access::Private, Some(a), Some(a)));
        assert!(!can_read(Access::Private, Some(a), Some(b)));
        assert!(can_read(Access::Private, Some(b), Some(b)));
        assert!(!can_read(Access::Private, Some(b), Some(a)));
    }

    #[test]
    fn can_write_matrix() {
        let a = OwnerId::new(1);
        let b = OwnerId::new(2);

        // Public: anyone may write, regardless of owner/requester.
        for owner in [None, Some(a)] {
            for requester in [None, Some(a), Some(b)] {
                assert!(
                    can_write(Access::Public, owner, requester),
                    "public write owner={owner:?} requester={requester:?}"
                );
            }
        }

        // Protected: only an exact owner match may write.
        assert!(can_write(Access::Protected, None, None)); // None == None
        assert!(!can_write(Access::Protected, None, Some(a)));
        assert!(!can_write(Access::Protected, Some(a), None));
        assert!(can_write(Access::Protected, Some(a), Some(a)));
        assert!(!can_write(Access::Protected, Some(a), Some(b)));
        assert!(can_write(Access::Protected, Some(b), Some(b)));
        assert!(!can_write(Access::Protected, Some(b), Some(a)));

        // Private: only an exact owner match may write.
        assert!(can_write(Access::Private, None, None)); // None == None
        assert!(!can_write(Access::Private, None, Some(a)));
        assert!(!can_write(Access::Private, Some(a), None));
        assert!(can_write(Access::Private, Some(a), Some(a)));
        assert!(!can_write(Access::Private, Some(a), Some(b)));
        assert!(can_write(Access::Private, Some(b), Some(b)));
        assert!(!can_write(Access::Private, Some(b), Some(a)));
    }

    #[test]
    fn access_matrix_grid_matches_db_masks() {
        // Exhaustive 3 access modes × owner ∈ {None, Some(a)} ×
        // requester ∈ {None, Some(a), Some(b)}, cross-checked against the legacy
        // status-mask model (DB_READ = PROTECTED|PUBLIC, DB_WRITE = PUBLIC) plus
        // the exact `requester == owner` fall-back in handle_lacking_permission.
        let a = OwnerId::new(1);
        let b = OwnerId::new(2);
        for access in [Access::Public, Access::Protected, Access::Private] {
            for owner in [None, Some(a)] {
                for requester in [None, Some(a), Some(b)] {
                    let owner_ok = requester == owner;
                    let expect_read = match access {
                        Access::Public | Access::Protected => true,
                        Access::Private => owner_ok,
                    };
                    let expect_write = match access {
                        Access::Public => true,
                        Access::Protected | Access::Private => owner_ok,
                    };
                    assert_eq!(
                        can_read(access, owner, requester),
                        expect_read,
                        "read access={access:?} owner={owner:?} requester={requester:?}"
                    );
                    assert_eq!(
                        can_write(access, owner, requester),
                        expect_write,
                        "write access={access:?} owner={owner:?} requester={requester:?}"
                    );
                }
            }
        }
    }
}
