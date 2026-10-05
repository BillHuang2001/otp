//! Table kinds, access modes and the parsed `ets:new/2` option set.
//!
//! Mirrors the option surface of the legacy `ets:new/2` (`db_new` in
//! `../../erts/emulator/beam/erl_db.c`): the `type` (table kind), `keypos`, the
//! `protection` (access mode), `named_table`, `compressed`,
//! `{write_concurrency, Bool}` and `{read_concurrency, Bool}` options. Parsing
//! the raw option list happens in `beam_bifs`; this module owns the **parsed**
//! representation ([`TableOptions`]) and its defaults.

use crate::table::OwnerId;
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

    /// The concurrency mode derived from the `read_concurrency` /
    /// `write_concurrency` flags (both set ⇒ [`Concurrency::ReadWrite`]).
    pub fn concurrency(&self) -> Concurrency {
        match (self.read_concurrency, self.write_concurrency) {
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
