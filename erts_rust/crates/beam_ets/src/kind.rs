//! Table kinds, access modes and the parsed `ets:new/2` option set.
//!
//! Mirrors the option surface of the legacy `ets:new/2` (`db_new` in
//! `../../erts/emulator/beam/erl_db.c`): the `type` (table kind), `keypos`, the
//! `protection` (access mode), `named_table`, `compressed`,
//! `{write_concurrency, Bool}` and `{read_concurrency, Bool}` options. Parsing
//! the raw option list happens in `beam_bifs`; this module owns the **parsed**
//! representation ([`TableOptions`]) and its defaults.

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

/// The parsed `ets:new/2` option set.
///
/// Built with the fluent builder ([`TableOptions::new`] plus the `set_*`
/// methods) and read through the getters. Defaults: [`TableKind::Set`],
/// `keypos` 1, [`Access::Protected`], not named, not compressed, no concurrency.
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
    /// The explicitly registered name (`Some` iff named).
    name: Option<Atom>,
    /// Whether `compressed` was requested.
    compressed: bool,
    /// Whether `write_concurrency` was requested.
    write_concurrency: bool,
    /// Whether `read_concurrency` was requested.
    read_concurrency: bool,
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

    /// Set the registered table name (implies a named table).
    pub fn set_name(&mut self, name: Atom) -> &mut Self {
        self.name = Some(name);
        self.named = true;
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

    /// Whether `write_concurrency` was requested.
    pub fn write_concurrency(&self) -> bool {
        self.write_concurrency
    }

    /// Whether `read_concurrency` was requested.
    pub fn read_concurrency(&self) -> bool {
        self.read_concurrency
    }
}

impl Default for TableOptions {
    fn default() -> TableOptions {
        TableOptions::new()
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

    #[test]
    fn chained_keypos_returns_self() {
        let mut opts = TableOptions::new();
        let returned = opts.set_keypos(2).unwrap();
        assert_eq!(returned.keypos(), 2);
        assert_eq!(opts.keypos(), 2);
    }

    #[test]
    fn set_name_implies_named() {
        let mut opts = TableOptions::new();
        assert!(!opts.is_named());

        let name = Atom::from_raw(7);
        opts.set_name(name);
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
}
