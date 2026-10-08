//! `ets:rename/2` — the table's **name** write, and the table identity a rename
//! must preserve.
//!
//! A rename changes *one* thing: the name a table reports. It must **not** change
//! the table's objects, its [`TableId`](super::TableId), its owner, its access
//! mode, its kind, its `keypos` or its interior — the table keeps answering every
//! other operation unchanged, and `ets:info/1,2` reports the new name.
//!
//! # The oracle's rules (probed with `erl`, OTP 29 / erts-17.1)
//!
//! Every rule below is the live oracle's answer; they are pinned by
//! `tests/ets_rename.rs`. The legacy C is `ets_rename_2` in
//! `../../erts/emulator/beam/erl_db.c`, which writes `tb->common.the_name` and
//! (only for a *named* table) rekeys the node's name registry.
//!
//! 1. **A `named_table`** (`ets:new(Name, [named_table])`):
//!    * a **free** new name ⇒ the name registry is rekeyed, the old name stops
//!      resolving (`ets:info(Old, name)` ⇒ `undefined`, `ets:whereis(Old)` ⇒
//!      `undefined`, any use of `Old` as a handle ⇒ `badarg`), the new name
//!      resolves to the *same* table identifier, and `ets:rename/2` returns the
//!      **new name atom**. The old name becomes free for a new table.
//!    * a new name that is **already registered** — by *another* named table or by
//!      this one itself — ⇒ `badarg`, and nothing changes (the C's
//!      `insert_named_tab` fails whenever the name is present, whatever table it
//!      points at). So **renaming a named table to the name it already has is
//!      `badarg`** too.
//!    * the *recorded* name of a table **without** `named_table` does **not**
//!      conflict: only named tables are in the registry, so a named table may take
//!      such a name and both tables then report the same `name` (while only the
//!      named one is resolvable by name).
//! 2. **A table without `named_table`** (`ets:new(Name, [])`, or a bare id):
//!    * the rename **always succeeds** — there is no conflict check at all, because
//!      such a table was never in the registry. That holds for a name that is
//!      registered to a *named* table (which keeps resolving to itself) and for the
//!      table's own current name.
//!    * the table is still **not registered**: `ets:info(T, name)` reports the new
//!      name, `ets:info(T, named_table)` stays `false` and `ets:whereis(NewName)`
//!      stays `undefined` (the C leaves `DB_NAMED_TABLE` untouched).
//!    * it returns the **unchanged table identifier** — not the name.
//! 3. **Write-gated, not owner-gated.** The C resolves the table with `DB_WRITE`:
//!    a non-owner may rename a `public` table, while a non-owner rename of a
//!    `protected`/`private` table is `badarg`. Match the gate of every other write
//!    ([`can_write`](crate::kind::can_write)); a rename is *not* an ownership
//!    operation like `ets:give_away/3`.
//! 4. Failures ⇒ the atom `badarg` (never a crash): an unknown/ deleted table
//!    identifier **or** name, and a new name that is **not an atom**.
//!
//! Rules 3 and 4's first half belong to the caller, not here: this primitive takes
//! an already-**interned atom** and an already-**resolved** [`Table`], so the
//! non-atom check and the name/tid lookup live in the BIF/registry layer
//! (`beam_bifs`'s `ets:*` bridge owns the name→table registry, as this crate holds
//! no global state).
//!
//! # What the caller (the `beam_bifs` registry) must do
//!
//! [`Table::rename`] performs the checked **name write** only. The caller's
//! registry, whose entries are `name atom -> TableId` for **`named_table`s only**,
//! must be rekeyed for the `RenameOutcome::Named` case and left **untouched** for
//! `RenameOutcome::Unnamed`:
//!
//! ```text
//! gate: !can_write(table.access(), table.owner(), requester)  =>  badarg
//! match table.rename(name, |n| registry.is_registered(n))? {
//!     RenameOutcome::Named { previous_name } => {
//!         registry.remove(previous_name);      // if any
//!         registry.insert(name, table.id());   // the new name
//!         reply(name_atom_term)                // the BIF returns the new NAME
//!     }
//!     RenameOutcome::Unnamed => reply(tid_term) // the BIF returns the unchanged TID
//! }
//! ```
//!
//! The `is_taken` predicate handed to [`Table::rename`] must report a name as
//! taken when **any named table** (including this one) is registered under it: the
//! oracle's `insert_named_tab` fails on *any* registry hit. The predicate is asked
//! **at most once**, and only for a `named_table` — an unnamed table has no
//! registry entry to conflict with (rule 2), so a caller whose registry is
//! maintained correctly can pass a predicate that ignores non-registered names
//! entirely.
//!
//! Gap: this crate cannot tell whether the registry is *consistent* with the
//! tables (there is no registry here), so it reports the conflict rule and the
//! rekey obligation; enforcing the one-name-per-table invariant end to end is the
//! registry's job.

use beam_common::Result;
use beam_term::prelude::*;

use super::Table;
use crate::error::badarg;

/// What a successful [`Table::rename`] means for the caller's name registry and
/// for `ets:rename/2`'s return value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    /// The table was created with `named_table`, so it **is** in the name
    /// registry: the caller must rekey it (drop `previous_name`, add the new
    /// name) and `ets:rename/2` returns the **new name atom**.
    Named {
        /// The name the table had before the rename (`None` when no name was
        /// recorded — the `named_table` flag and the recorded name are
        /// independent).
        previous_name: Option<Atom>,
    },
    /// The table was created **without** `named_table`, so it was never in the
    /// registry: the caller must leave the registry untouched and `ets:rename/2`
    /// returns the **unchanged table identifier**.
    Unnamed,
}

impl Table {
    /// The checked **name write** of `ets:rename/2`: give this table the name
    /// `name`, keeping its objects, its [`TableId`](super::TableId), its owner,
    /// its access mode and its interior.
    ///
    /// `is_taken(name)` reports whether `name` is already registered *by any named
    /// table, including this one* (the caller's registry is the authority — see
    /// the module docs for the contract). It is consulted **only** when this table
    /// has `named_table`; an unnamed table's rename has no conflict rule at all
    /// (rule 2).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`](beam_common::BeamError::BadArg), leaving the table
    /// completely unchanged, when this table has `named_table` **and** `name` is
    /// already taken — which includes `name` being the table's own current name
    /// (the oracle's registry lookup hits either way).
    ///
    /// The other two `ets:rename/2` failures — a non-atom name, and an unknown/
    /// dead table — are the caller's (this primitive takes an interned [`Atom`]
    /// and an already-resolved handle), as is the write gate.
    pub fn rename(
        &mut self,
        name: Atom,
        is_taken: impl FnOnce(Atom) -> bool,
    ) -> Result<RenameOutcome> {
        if !self.opts.is_named() {
            // Not a named table: the oracle does no conflict check whatsoever
            // (the table is not in the registry), it only overwrites the name.
            self.opts.set_name(name);
            return Ok(RenameOutcome::Unnamed);
        }
        // A named table: the new name must be free. The table's own name counts as
        // taken (the C's `insert_named_tab` finds the entry it already owns), so a
        // named table cannot be renamed to the name it already has.
        if self.name() == Some(name) || is_taken(name) {
            return Err(badarg());
        }
        let previous_name = self.name();
        self.opts.set_name(name);
        Ok(RenameOutcome::Named { previous_name })
    }
}
