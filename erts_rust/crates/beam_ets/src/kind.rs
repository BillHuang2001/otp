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
        unimplemented!("M1: implementation pending")
    }

    /// Whether the kind stores multiple objects per key (`bag`,
    /// `duplicate_bag`). `false` for `set`/`ordered_set`.
    pub fn is_multi(&self) -> bool {
        unimplemented!("M1: implementation pending")
    }

    /// Whether the kind is ordered (`ordered_set`). `false` otherwise.
    pub fn is_ordered(&self) -> bool {
        unimplemented!("M1: implementation pending")
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
        unimplemented!("M1: implementation pending")
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
        unimplemented!("M1: implementation pending")
    }

    /// Set the table kind.
    pub fn set_kind(&mut self, kind: TableKind) -> &mut Self {
        unimplemented!("M1: implementation pending")
    }

    /// Set the 1-based `keypos`. Rejects `0` with `badarg`.
    pub fn set_keypos(&mut self, keypos: usize) -> Result<&mut Self> {
        unimplemented!("M1: implementation pending")
    }

    /// Set the access mode (`protection`).
    pub fn set_access(&mut self, access: Access) -> &mut Self {
        unimplemented!("M1: implementation pending")
    }

    /// Set whether the table is named (`named_table`).
    pub fn set_named(&mut self, named: bool) -> &mut Self {
        unimplemented!("M1: implementation pending")
    }

    /// Set the registered table name (implies a named table).
    pub fn set_name(&mut self, name: Atom) -> &mut Self {
        unimplemented!("M1: implementation pending")
    }

    /// Set whether stored terms are compressed (`compressed`).
    pub fn set_compressed(&mut self, compressed: bool) -> &mut Self {
        unimplemented!("M1: implementation pending")
    }

    /// Set whether concurrent writers are allowed (`write_concurrency`).
    pub fn set_write_concurrency(&mut self, write: bool) -> &mut Self {
        unimplemented!("M1: implementation pending")
    }

    /// Set whether concurrent readers are allowed (`read_concurrency`).
    pub fn set_read_concurrency(&mut self, read: bool) -> &mut Self {
        unimplemented!("M1: implementation pending")
    }

    /// The configured table kind.
    pub fn kind(&self) -> TableKind {
        unimplemented!("M1: implementation pending")
    }

    /// The configured 1-based key position.
    pub fn keypos(&self) -> usize {
        unimplemented!("M1: implementation pending")
    }

    /// The configured access mode.
    pub fn access(&self) -> Access {
        unimplemented!("M1: implementation pending")
    }

    /// Whether the table is named.
    pub fn is_named(&self) -> bool {
        unimplemented!("M1: implementation pending")
    }

    /// The configured table name, if any.
    pub fn name(&self) -> Option<Atom> {
        unimplemented!("M1: implementation pending")
    }

    /// Whether compression was requested.
    pub fn is_compressed(&self) -> bool {
        unimplemented!("M1: implementation pending")
    }

    /// Whether `write_concurrency` was requested.
    pub fn write_concurrency(&self) -> bool {
        unimplemented!("M1: implementation pending")
    }

    /// Whether `read_concurrency` was requested.
    pub fn read_concurrency(&self) -> bool {
        unimplemented!("M1: implementation pending")
    }
}

impl Default for TableOptions {
    fn default() -> TableOptions {
        unimplemented!("M1: implementation pending")
    }
}
