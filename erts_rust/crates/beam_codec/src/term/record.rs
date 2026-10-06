//! Record forwarding layer over [`beam_term::record`].
//!
//! The frozen interior layout of a **native record** (OTP 29's
//! `ErtsRecordDefinition` / `ErtsRecordInstance`) — the `[field_order tuple]
//! [definition][instance]` construction block, the definition/key/value word
//! offsets and the declaration-vs-sorted-key rule — is owned by
//! [`beam_term::record`] (the lowest common ancestor for term layouts — workspace
//! `CONTEXT.md` §2, "No duplication across crates"). This module only:
//!
//! 1. checks the documented type/validity preconditions, mapping a failure to a
//!    [`beam_common::BeamError`] (`TermError::BadRecord` ⇒ `BadArg`), and
//! 2. allocates the block from [`beam_alloc::Heap`] ([`make_record`]) or forwards
//!    straight to the matching [`beam_term::record`] accessor.
//!
//! All layout arithmetic — the sort rule, `field_order`, the word offsets — lives
//! in [`beam_term::record`].
//!
//! This is a child of the [`crate::term`] subtree, so it inherits that subtree's
//! `#[allow(unsafe_code)]` (the crate's single, auditable `unsafe` surface under the
//! crate-wide `#![deny(unsafe_code)]`): the [`beam_alloc`] allocation calls, the
//! [`beam_term`] forwarders and the minimal validity guards — each carrying a
//! `// SAFETY:` rationale.
//!
//! Every error stays [`beam_common::Result`] (via [`crate::error`]); a reader never
//! panics and never exhibits undefined behaviour on a mismatched term.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{make_small, Atom, Eterm};
use beam_term::record;

/// The atoms `true`/`false` a record's `is_exported` flag stores — the standard
/// `atom.names` indices `AM_FALSE`/`AM_TRUE` (`beam_term::record`).
pub use beam_term::record::{AM_FALSE, AM_TRUE};

/// The total words [`make_record`] reserves for a `field_count`-field record
/// (`beam_term::record::record_block_words`).
pub use beam_term::record::{record_block_words, record_def_size_words, record_inst_size_words};

/// Whether `term` is a native-record instance (`is_record`).
///
/// Only the boxed pointer tag and the word's header subtag are examined, so a
/// small, `NIL` or any non-record term is reported as `false` without a read.
pub fn is_record(term: Eterm) -> bool {
    // SAFETY: `is_record` reads only the header word of a boxed term, which is the
    // standing invariant for any `Eterm`; a non-boxed term is rejected without a read.
    unsafe { beam_term::boxed::is_record(term) }
}

/// Materializes a native record on `heap`.
///
/// `fields` are `(key-atom-term, value)` pairs in **declaration** order; the block
/// written by [`beam_term::record::write_record`] sorts the keys by atom index and
/// records the declaration→sorted rank in `field_order`.
///
/// A duplicate key, a non-atom key or a field count whose definition arity would
/// overflow `MAX_ARITYVAL` yields `BeamError::BadArg` (via `TermError::BadRecord`),
/// never a corrupt block.
///
/// **Cached definition hash (documented deviation):** C stores
/// `make_hash2({module,name,is_exported,keys})` at definition word 2. `beam_term`
/// cannot reproduce `make_hash2` (its [`beam_term::hashing`] is explicitly not
/// bit-exact), so the small integer `0` is stored instead. The hash is **not** on the
/// wire — the ETF decoder computes it from the decoded keys — so this is not
/// wire-observable; a caller that needs `phash2` compatibility must supply the C
/// value (this forwarding layer does not expose that escape hatch).
pub fn make_record(
    heap: &mut Heap,
    module: Atom,
    name: Atom,
    is_exported: bool,
    fields: &[(Eterm, Eterm)],
) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `record_block_words(fields.len())`
    // exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(record_block_words(fields.len()))? };
    // SAFETY: `hp` owns exactly those words and `fields` are the declaration-order
    // `(key, value)` pairs `write_record` documents. The definition hash is the
    // documented deviation above.
    Ok(unsafe { record::write_record(hp, module, name, is_exported, fields, make_small(0))? })
}

/// The number of fields of a native-record instance term.
///
/// `BeamError::BadArg` for a non-record or malformed term.
pub fn record_field_count(term: Eterm) -> Result<usize> {
    // SAFETY: the caller guarantees a live boxed term; `record_field_count` rejects a
    // non-record without dereferencing it.
    Ok(unsafe { record::record_field_count(term)? })
}

/// The `record_definition` word of a native-record instance term — a boxed pointer to
/// the definition block.
///
/// `BeamError::BadArg` for a non-record or malformed term.
pub fn record_definition(term: Eterm) -> Result<Eterm> {
    // SAFETY: as `record_field_count`.
    Ok(unsafe { record::record_definition(term)? })
}

/// The field **value** words of a native-record instance, in **sorted-key** order.
///
/// The slice is borrowed from the term's storage (the caller must keep the owning
/// heap alive and unmoved). `BeamError::BadArg` for a non-record.
pub fn record_values<'a>(term: Eterm) -> Result<&'a [Eterm]> {
    // SAFETY: the caller guarantees a live boxed term; `record_values` rejects a
    // non-record without dereferencing it, and the caller keeps the heap alive for `'a`.
    Ok(unsafe { record::record_values(term)? })
}

/// The definition's `module` atom term.
///
/// `BeamError::BadArg` for a non-definition.
pub fn record_definition_module(def: Eterm) -> Result<Eterm> {
    // SAFETY: the caller guarantees a live boxed term; `definition_parts` rejects a
    // malformed definition without dereferencing past its header.
    Ok(unsafe { record::record_definition_module(def)? })
}

/// The definition's `name` atom term.
///
/// `BeamError::BadArg` for a non-definition.
pub fn record_definition_name(def: Eterm) -> Result<Eterm> {
    // SAFETY: as `record_definition_module`.
    Ok(unsafe { record::record_definition_name(def)? })
}

/// The definition's `is_exported` atom term ([`AM_TRUE`] or [`AM_FALSE`]).
///
/// `BeamError::BadArg` for a non-definition.
pub fn record_definition_is_exported(def: Eterm) -> Result<Eterm> {
    // SAFETY: as `record_definition_module`.
    Ok(unsafe { record::record_definition_is_exported(def)? })
}

/// The definition's field **keys** (atom terms) in ascending atom-index order.
///
/// The slice is borrowed from the definition's storage (the caller must keep the
/// owning heap alive and unmoved).
pub fn record_definition_keys<'a>(def: Eterm) -> Result<&'a [Eterm]> {
    // SAFETY: as `record_definition_module`; the caller keeps the heap alive for `'a`.
    Ok(unsafe { record::record_definition_keys(def)? })
}

/// The definition's `field_order` elements (small integers) — `order[i]` is the
/// sorted rank of the field declared `i`-th.
///
/// The slice is borrowed from the definition's storage (the caller must keep the
/// owning heap alive and unmoved). `BeamError::BadArg` for a malformed `field_order`.
pub fn record_definition_field_order<'a>(def: Eterm) -> Result<&'a [Eterm]> {
    // SAFETY: as `record_definition_module`; the caller keeps the heap alive for `'a`.
    Ok(unsafe { record::record_definition_field_order(def)? })
}

/// The definition's keys in **declaration** order
/// (`declaration_keys[i] == keys[unsigned_val(order[i])]`).
///
/// `BeamError::BadArg` for a malformed definition or `field_order`.
pub fn record_declaration_keys(def: Eterm) -> Result<Vec<Eterm>> {
    // SAFETY: as `record_definition_module`.
    Ok(unsafe { record::record_declaration_keys(def)? })
}

#[cfg(test)]
mod tests {
    //! Forwarding tests: the raw-word layout is asserted by `beam_term::record`; here
    //! we only check that the forwarding layer allocates the quoted number of words,
    //! round-trips every reader and maps a malformed term to `BeamError::BadArg`.

    use beam_term::prelude::{atom_val, make_atom, make_small, AtomTable, NIL};

    use super::*;

    /// A table seeded like the standard `atom.names` (`false`, `true` first).
    fn atoms() -> AtomTable {
        let mut table = AtomTable::new();
        table.seed(["false", "true"]);
        table.seed(["m", "r", "a", "b", "c"]);
        table
    }

    #[test]
    fn make_record_round_trips_every_reader() {
        let t = atoms();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();
        let a = t.lookup("a").unwrap();
        let b = t.lookup("b").unwrap();

        let mut heap = Heap::new(record_block_words(2)).expect("a sized heap");
        // Declared [(b, 22), (a, 11)] — reverse of the atom-index order.
        let fields = [
            (make_atom(b), make_small(22)),
            (make_atom(a), make_small(11)),
        ];
        let rec = make_record(&mut heap, m, r, true, &fields).unwrap();

        assert_eq!(heap.used(), record_block_words(2), "the quoted block is used");
        assert!(is_record(rec));
        assert_eq!(record_field_count(rec).unwrap(), 2);

        let def = record_definition(rec).unwrap();
        assert_eq!(atom_val(record_definition_module(def).unwrap()), m);
        assert_eq!(atom_val(record_definition_name(def).unwrap()), r);
        assert_eq!(
            record_definition_is_exported(def).unwrap(),
            make_atom(AM_TRUE)
        );

        // Sorted-key order: `a` (index smaller) precedes `b`.
        let keys = record_definition_keys(def).unwrap();
        assert_eq!(atom_val(keys[0]), a);
        assert_eq!(atom_val(keys[1]), b);
        // Values are stored in that same sorted order.
        let values = record_values(rec).unwrap();
        assert_eq!(values, &[make_small(11), make_small(22)]);
        // Declaration order is recovered through `field_order`: declared 0 is `b`.
        let decl = record_declaration_keys(def).unwrap();
        assert_eq!(atom_val(decl[0]), b);
        assert_eq!(atom_val(decl[1]), a);
        let order = record_definition_field_order(def).unwrap();
        assert_eq!(order, &[make_small(1), make_small(0)]);
    }

    #[test]
    fn readers_reject_non_records() {
        let t = atoms();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();
        let mut heap = Heap::new(64).expect("a small heap");
        let tuple = heap.tuple(&[make_small(1), make_small(2)]).unwrap();

        for term in [make_small(1), NIL, tuple] {
            assert!(!is_record(term));
            assert!(matches!(record_field_count(term), Err(beam_common::BeamError::BadArg)));
            assert!(matches!(record_definition(term), Err(beam_common::BeamError::BadArg)));
            assert!(matches!(record_values(term), Err(beam_common::BeamError::BadArg)));
            assert!(matches!(record_definition_module(term), Err(beam_common::BeamError::BadArg)));
            assert!(matches!(record_definition_name(term), Err(beam_common::BeamError::BadArg)));
            assert!(matches!(record_definition_keys(term), Err(beam_common::BeamError::BadArg)));
            assert!(matches!(record_declaration_keys(term), Err(beam_common::BeamError::BadArg)));
        }

        // A duplicate key is rejected at construction (never a corrupt block).
        let mut heap = Heap::new(record_block_words(2)).expect("a sized heap");
        let dup = [(make_atom(m), make_small(1)), (make_atom(m), make_small(2))];
        assert!(matches!(
            make_record(&mut heap, m, r, false, &dup),
            Err(beam_common::BeamError::BadArg)
        ));

        // A non-atom key is rejected too.
        let mut heap = Heap::new(record_block_words(1)).expect("a sized heap");
        let bad_key = [(make_small(1), make_small(2))];
        assert!(matches!(
            make_record(&mut heap, m, r, false, &bad_key),
            Err(beam_common::BeamError::BadArg)
        ));
    }
}
