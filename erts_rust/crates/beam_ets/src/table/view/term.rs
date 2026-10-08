//! [`TableTerm`] — a table-owned term that may not outlive the view that read it.
//!
//! A `TableTerm` is deliberately *not* a term: it exposes read-only accessors, an
//! explicit [`copy_into`](TableTerm::copy_into) and nothing that yields a bare,
//! escaping [`Eterm`]. Its lifetime is tied to the borrow of the
//! [`TableRead`](super::TableRead)/[`TableWrite`](super::TableWrite) view, which
//! owns the table's lock guard — so the term and the guard live and die together.
//! See the [module docs](super) for the aliasing hazard this prevents.

use core::cmp::Ordering;
use core::marker::PhantomData;

use beam_common::Result;
use beam_term::prelude::*;

use crate::store::TermStore;
use crate::table::Table;

/// A term read out of table-owned storage under a live table guard.
///
/// The `'g` lifetime is the **borrow of the reading view**, never the view's own
/// lifetime parameter: a `TableTerm` therefore cannot be returned from a
/// `with_read`/`with_write` closure, and cannot be kept after the view — and with
/// it the `RwLock` guard — is dropped. Handing it out any other way is a compile
/// error:
///
/// ```compile_fail
/// use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions};
/// use beam_term::prelude::*;
///
/// let mut opts = TableOptions::new();
/// opts.set_kind(TableKind::Set).set_access(Access::Public);
/// let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
/// let atoms = AtomTable::new();
///
/// // This does *not* compile: the term borrows the closure-local view, which
/// // owns the read guard, so it may not escape the closure.
/// let leaked = shared.with_read(|view| view.lookup(make_small(1), &atoms)).unwrap();
/// let _ = leaked.len();
/// ```
///
/// Reading a term and *owning* it requires the explicit, allocating
/// [`copy_into`](TableTerm::copy_into):
///
/// ```
/// use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions, TermStore};
/// use beam_term::prelude::*;
///
/// let mut opts = TableOptions::new();
/// opts.set_kind(TableKind::Set).set_access(Access::Public);
/// let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
/// let atoms = AtomTable::new();
///
/// let mut source = Heap::new(8);
/// let object = source.tuple(&[make_small(1), make_small(10)]).unwrap();
/// shared.with_write(|view| view.insert(object, &atoms)).unwrap().unwrap();
///
/// // The copy lives in a caller-owned `TermStore`, so it outlives the guard.
/// let mut owned = TermStore::with_default_allocator();
/// let copy = shared
///     .with_read(|view| {
///         let hits = view.lookup(make_small(1), &atoms);
///         hits[0].copy_into(&mut owned)
///     })
///     .unwrap()
///     .unwrap();
///
/// // A `{1, 10}` 2-tuple is a header plus two element words.
/// assert_eq!(size_object(copy), 3);
/// ```
pub struct TableTerm<'g> {
    /// The raw term word, read out of table-owned storage.
    term: Eterm,
    /// Ties the term to the reading view's borrow `'g`. `&Table` (rather than
    /// `&mut`) keeps the marker covariant in `'g`, so a longer-lived term may be
    /// shortened but never widened — exactly the direction that keeps an escaped
    /// term from compiling.
    _scope: PhantomData<&'g Table>,
}

impl<'g> TableTerm<'g> {
    /// Wrap a raw term read out of table storage.
    ///
    /// Crate-internal: the `TableRead`/`TableWrite` views are the only producers
    /// of a `TableTerm`, which is what makes the lifetime discipline airtight.
    pub(super) fn new(term: Eterm) -> TableTerm<'g> {
        TableTerm {
            term,
            _scope: PhantomData,
        }
    }

    /// The term's virtual class (`beam_term::class`).
    fn term_class(&self) -> TermClass {
        // SAFETY: the term was read out of a live Eterm slot under the view's
        // guard, so it is a live, well-formed term and its header word (if boxed)
        // is a valid, aligned term word.
        unsafe { class(self.term) }
    }

    // ---------------------------------------------------------------- type tests

    /// Whether the term is an integer (a small integer or a bignum).
    pub fn is_integer(&self) -> bool {
        matches!(self.term_class(), TermClass::Small | TermClass::Big)
    }

    /// Whether the term is a float.
    pub fn is_float(&self) -> bool {
        matches!(self.term_class(), TermClass::Float)
    }

    /// Whether the term is an atom.
    pub fn is_atom(&self) -> bool {
        matches!(self.term_class(), TermClass::Atom)
    }

    /// Whether the term is a tuple.
    pub fn is_tuple(&self) -> bool {
        matches!(self.term_class(), TermClass::Tuple)
    }

    /// Whether the term is a list (a cons cell or the empty list).
    pub fn is_list(&self) -> bool {
        matches!(self.term_class(), TermClass::List | TermClass::Nil)
    }

    /// Whether the term is a bitstring (a binary or a non-byte-aligned bitstring).
    pub fn is_bitstring(&self) -> bool {
        // SAFETY: as `term_class` — `is_bitstring` reads a boxed header only when
        // the word is a boxed term.
        unsafe { beam_term::boxed::is_bitstring(self.term) }
    }

    /// Whether the term is a binary (a bitstring whose size is a whole number of
    /// bytes).
    pub fn is_binary(&self) -> bool {
        // SAFETY: as `term_class`.
        unsafe { beam_term::boxed::is_binary(self.term) }
    }

    /// Whether the term is a map.
    pub fn is_map(&self) -> bool {
        matches!(self.term_class(), TermClass::Map)
    }

    /// Whether the term is a pid (internal or external).
    pub fn is_pid(&self) -> bool {
        matches!(self.term_class(), TermClass::Pid | TermClass::ExternalPid)
    }

    /// Whether the term is a port (internal or external).
    pub fn is_port(&self) -> bool {
        matches!(self.term_class(), TermClass::Port | TermClass::ExternalPort)
    }

    /// Whether the term is a reference (internal or external).
    pub fn is_reference(&self) -> bool {
        matches!(self.term_class(), TermClass::Ref | TermClass::ExternalRef)
    }

    /// Whether the term is a fun.
    pub fn is_function(&self) -> bool {
        matches!(self.term_class(), TermClass::Fun)
    }

    // ----------------------------------------------------------- value accessors

    /// The value of a **small** integer term; `None` for anything else (including a
    /// bignum, which no fixed-width accessor can report faithfully).
    pub fn as_small_int(&self) -> Option<i64> {
        if is_small(self.term) {
            Some(signed_value(self.term))
        } else {
            None
        }
    }

    /// The atom index of an atom term; `None` otherwise.
    pub fn as_atom(&self) -> Option<Atom> {
        if is_atom(self.term) {
            Some(atom_val(self.term))
        } else {
            None
        }
    }

    /// The value of a float term; `None` otherwise.
    pub fn as_float(&self) -> Option<f64> {
        if self.is_float() {
            // SAFETY: `is_float` established a live boxed float.
            Some(unsafe { float_value(self.term) })
        } else {
            None
        }
    }

    /// The arity of a tuple term; `None` for anything that is not a tuple.
    pub fn tuple_arity(&self) -> Option<usize> {
        if self.is_tuple() {
            // SAFETY: `is_tuple` established a live tuple.
            Some(unsafe { tuple_size(self.term) })
        } else {
            None
        }
    }

    /// Element `i` (0-based) of a tuple term; `None` when the term is not a tuple
    /// or `i` is out of range.
    pub fn tuple_element(&self, i: usize) -> Option<TableTerm<'g>> {
        match self.tuple_arity() {
            Some(arity) if i < arity => {
                // SAFETY: the term is a live tuple of `arity > i` elements.
                Some(TableTerm::new(unsafe { tuple_element(self.term, i) }))
            }
            _ => None,
        }
    }

    /// The elements of a list term, head first.
    ///
    /// A proper list yields all its elements; an **improper** list yields its
    /// proper prefix followed by the non-list tail as the final element (so the
    /// tail is never silently dropped). A non-list term yields a one-element
    /// vector holding the term itself.
    pub fn list_to_terms(&self) -> Vec<TableTerm<'g>> {
        let mut elements = Vec::new();
        let mut cursor = self.term;
        while is_list(cursor) {
            // SAFETY: `is_list` established a live cons cell, so both words are
            // initialised terms.
            let head = unsafe { car(cursor) };
            let tail = unsafe { cdr(cursor) };
            elements.push(TableTerm::new(head));
            cursor = tail;
        }
        if !is_nil(cursor) {
            // An improper tail (or a non-list term): keep it as the last element.
            elements.push(TableTerm::new(cursor));
        }
        elements
    }

    /// The term's size in machine words (`beam_term::size_object`).
    ///
    /// A cheap, allocation-free measure of how much storage the object occupies —
    /// handy for estimating a copy-out's cost.
    pub fn size(&self) -> usize {
        size_object(self.term)
    }

    // ----------------------------------------------------------- comparisons

    /// Whether this term equals `other` under `=:=` (`beam_term::compare::eq`).
    ///
    /// `other` is a plain [`Eterm`] the *caller* owns: comparing a table term
    /// against a caller-built term is the common "did I read back what I wrote?"
    /// check and never needs the table term to escape.
    pub fn eq_term(&self, other: Eterm, atoms: &AtomTable) -> bool {
        eq(self.term, other, atoms)
    }

    /// The term-order comparison of this term against `other`
    /// (`beam_term::compare::cmp`).
    pub fn cmp_term(&self, other: Eterm, atoms: &AtomTable) -> Ordering {
        cmp(self.term, other, atoms)
    }

    // -------------------------------------------------------------- copy-out

    /// Deep-copies the term into `dest` and returns the copy's root.
    ///
    /// This is the **explicit, allocating** way to obtain a term that may outlive
    /// the guard. It reuses the table's own deep-copier
    /// ([`TermStore::copy`](crate::store::TermStore::copy)), so its supported
    /// subset is exactly what an `insert` can store — no capability regression —
    /// and an unsupported kind (a fun, an external pid, …) is the same typed
    /// `badarg` an insert would give.
    pub fn copy_into(&self, dest: &mut TermStore) -> Result<Eterm> {
        dest.copy(self.term)
    }
}

impl core::fmt::Debug for TableTerm<'_> {
    /// Renders the term's class only — never the term itself, so a stale (if it
    /// could exist) or hostile word is never interpreted.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "TableTerm({:?})", self.term_class())
    }
}
