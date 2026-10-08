//! Erlang term ordering — the lowest-common-ancestor `erts_cmp`.
//!
//! Reproduces `erts_cmp(a, b, exact, eq_only)` (an inline helper in
//! `erts/emulator/beam/erl_utils.h`) together with its deep core
//! `erts_cmp_compound(a, b, exact, eq_only)` (`erts/emulator/beam/utils.c`), the
//! function `lists:sort/1`, `ordered_set` tables and the `<`/`>` BIFs are built
//! on. It is defined here so `beam_ets` (ordered_set ordering), `beam_bifs`
//! (comparison operators) and others do not each re-implement it.
//!
//! [`cmp`] reproduces the **exact** comparison `erts_cmp(a, b, exact=1, eq_only=0)`
//! used by `=:=` (so `1` and `1.0` are *not* equal, and `-0.0 < +0.0`), and
//! [`cmp_arith`] reproduces the arithmetic comparison `exact=0` used by `==`.
//!
//! # Term order
//!
//! Ascending term order is
//! `number < atom < reference < fun < port < pid < tuple < record < map < nil < list < bitstring`
//! (plus the external variants `external_ref`/`external_port`/`external_pid`
//! interleaved after their internal counterpart). This is derived directly from
//! the `_DEF` virtual-tag constants in `erts/emulator/beam/erl_term.h` — term
//! order ascends as `_DEF` descends (`rank = 0x11 - _DEF`), matching
//! `beam_term::ops::TermClass`'s documented order.
//!
//! ## Known Issues (documented comparison gaps)
//!
//! The C `erts_cmp_compound` navigates every internal layout. This port
//! faithfully covers the classes the rewrite needs today and provides an
//! explicit, *deterministic* and panic-free fallback for the rest:
//!
//! * **Atoms** are compared by name bytes (reproducing `erts_cmp_atoms`'s
//!   memcmp-of-names + length tie-break). If an atom's name is unknown to the
//!   passed [`AtomTable`] the comparison falls back to the raw atom index.
//! * **Numbers** (`small`, `big`, `float`) compare numerically: small/small via
//!   the sign-extended value, small/big via the big's sign (a bignum is always
//!   outside the small range), big/big limb-by-limb (most-significant first), and
//!   float/float via the total `erts_float_exact_sortable` order in exact mode
//!   (so `-0.0 < +0.0` and NaN has a stable position) or IEEE comparison in
//!   arithmetic mode. In *exact* mode a mixed integer/float pair (`1` vs `1.0`)
//!   compares by class rank, not numerically — exactly as `erts_cmp` does. In
//!   *arithmetic* mode a mixed integer/float pair instead compares via the
//!   best-effort `f64` conversion
//!   [`crate::number::number_as_f64`] (faithful nearest-double rounding for a
//!   representable bignum; an unrepresentable magnitude becomes `±inf`).
//! * **Tuples** compare elementwise after the arity; **lists** (cons cells)
//!   compare elementwise with the shorter/improper tail deciding a prefix tie;
//!   **bitstrings** compare bit-by-bit (most-significant first) then by bit size;
//!   **internal pids/ports** compare by their raw immediate payload.
//! * **Internal references** are compared by their raw payload words
//!   (most-significant first). The C compares the decoded `num[]` array; the
//!   payload-word comparison is a deterministic proxy that is equivalent as long
//!   as the platform ref layout is stable.
//! * **Funs** are compared structurally per [`cmp_funs`]: within a kind by the
//!   inlined entry fields the descriptor carries, then `num_free`, then the
//!   environment; `External` sorts after any local kind. The `Local` kind has no
//!   `module`/`old_uniq` in its M1 descriptor and mixed `Local`/`Imported` pairs
//!   lead with the kind rank — see [`cmp_funs`] for both documented deviations.
//! * **External pids/ports/refs and native records** are compared *structurally*
//!   (see [`cmp_external_pids`], [`cmp_external_ports`], [`cmp_external_refs`] and
//!   [`cmp_records`]), so a copy of such a term compares `=:=`/`Equal` to its
//!   original. The field orders reproduce `erts_cmp_compound`: an external pid by
//!   `serial`, then `number`, then the node (sysname atom, then `creation`); an
//!   external port by the node, then `number`; an external reference by the node,
//!   then its id words as one big integer (highest index most significant, the
//!   longer slice's leading zero ids trimmed first); a native record by its
//!   definition's `module`, `name` and `is_exported` (`false < true`), its field
//!   count and keys, and finally its field values.
//! * **On-heap bin refs** have a *deterministic fallback*: they are ordered by
//!   class first (which is exact), then by object arity, then by the raw term word
//!   (a stable proxy). This never panics but is not a faithful reproduction of the
//!   C's structural comparison; a caller that needs exact ordering for it must
//!   extend this module.
//!
//! ## Non-term words (a documented robustness extension)
//!
//! The C `erts_cmp`/`erts_cmp_compound` assume both operands are **terms**. A
//! word that is not a term at all — one whose primary tag is `HEADER`, i.e. the C
//! ABI's `THE_NON_VALUE` (`0`, or a bare `FLOAT` header in a debug build) and any
//! other stray header word (a NIF returning `enif_make_copy`'s "no value", say) —
//! makes the C dereference a garbage (in release, `NULL`) `boxed_val` pointer:
//! undefined behaviour. [`cmp_impl`] instead guards on
//! [`crate::tag::is_term_word`] *before* any classification or dereference:
//!
//! * a non-term sorts **after** every well-formed term, so it never compares
//!   `Equal` to one;
//! * two non-terms order by their raw word (so the relation stays a total order
//!   and `Equal` means "the identical word", preserving `eq ⇒ equal hash`).
//!
//! This is a documented **hardening**, not a behavioural change for valid input:
//! no valid term has the `HEADER` primary tag, so every comparison of terms is
//! bit-for-bit what it was.

use core::cmp::Ordering;

use crate::atom::AtomTable;
use crate::bitstring::{bit_at, bitstring_parts, float_bits};
use crate::boxed::{boxed_val, header_arity, is_float};
use crate::eterm::Eterm;
use crate::fun::{fun_arity, fun_desc_words, fun_kind_of_header, fun_num_free, FunKind};
use crate::immediate::{
    atom_val, internal_pid_data, internal_port_data, is_atom, is_small, signed_value,
};
use crate::list::{car, cdr, is_list};
use crate::number::number_as_f64;
use crate::ops::{class, TermClass};
use crate::record::{
    record_definition, record_definition_is_exported, record_definition_keys,
    record_definition_module, record_definition_name, record_field_count, record_values,
};
use crate::tag::{is_term_word, BIG_SIGN_BIT};
use crate::thing::{external_pid_parts, external_port_parts, external_ref_parts};
use crate::tuple::{tuple_element, tuple_size};

/// Ascending term-order rank of a [`TermClass`].
///
/// `rank = 0x11 - _DEF`, so a smaller rank sorts first. See the module docs.
fn class_rank(class: TermClass) -> u8 {
    match class {
        TermClass::Small => 0,
        TermClass::Big => 1,
        TermClass::Float => 2,
        TermClass::Atom => 3,
        TermClass::ExternalRef => 4,
        TermClass::Ref => 5,
        TermClass::Fun => 6,
        TermClass::ExternalPort => 7,
        TermClass::Port => 8,
        TermClass::ExternalPid => 9,
        TermClass::Pid => 10,
        TermClass::Tuple => 11,
        TermClass::Record => 12,
        TermClass::Map => 13,
        TermClass::Nil => 14,
        TermClass::List => 15,
        TermClass::Bitstring => 16,
        TermClass::BinRef => 17,
        // A word that is not a term at all (a bare header word — the ABI's
        // `THE_NON_VALUE` and friends): documented as sorting **after** every real
        // term, so it can never compare `Equal` to one. See [`cmp_impl`].
        TermClass::NonValue => 18,
    }
}

/// Whether a class is one of the three numeric classes.
const fn is_number_class(class: TermClass) -> bool {
    matches!(
        class,
        TermClass::Small | TermClass::Big | TermClass::Float
    )
}

/// The exact (`=:=`) ordering of two terms — `erts_cmp(a, b, exact=1, eq_only=0)`.
///
/// `atoms` supplies the atom names needed for [`cmp_atoms`]-style comparison; a
/// name that is not interned falls back to the raw atom index.
///
/// A word that is **not a term** (a bare header word — the ABI's `THE_NON_VALUE`
/// and friends) is not dereferenced: it sorts after every real term, and two such
/// words order by their raw word. See the module docs and [`cmp_impl`].
pub fn cmp(a: Eterm, b: Eterm, atoms: &AtomTable) -> Ordering {
    cmp_impl(a, b, atoms, true)
}

/// `=:=` — whether two terms are exactly equal (`cmp(a, b).is_eq()`).
///
/// A non-term word is exactly equal only to the identical word (never to a real
/// term) — see [`cmp`].
pub fn eq(a: Eterm, b: Eterm, atoms: &AtomTable) -> bool {
    cmp(a, b, atoms).is_eq()
}

/// The arithmetic (`==`) ordering — `erts_cmp(a, b, exact=0, eq_only=0)`.
///
/// Differs from [`cmp`] only where `==` and `=:=` diverge: floats use IEEE
/// comparison (so `-0.0 == +0.0`), and a mixed integer/float pair compares
/// numerically (`1 == 1.0`). Bignum-to-float conversion is a documented
/// approximation (see the module `Known Issues`). A non-term word is handled
/// exactly as in [`cmp`].
pub fn cmp_arith(a: Eterm, b: Eterm, atoms: &AtomTable) -> Ordering {
    cmp_impl(a, b, atoms, false)
}

/// The shared comparison core, parameterised by `exact`.
fn cmp_impl(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    // Identical words are always equal (the C `is_same` fast path).
    if a.raw() == b.raw() {
        return Ordering::Equal;
    }
    // A word that is not a term at all — a bare header word, i.e. the C ABI's
    // `THE_NON_VALUE` (`0`, or a bare `FLOAT` header word in a debug build) or any
    // other word whose primary tag is `HEADER` (see `tag::is_term_word`) — must be
    // filtered out *here*, before the classification and the `is_float` /
    // `boxed_val` dereference below. `is_boxed` mirrors the C bit trick and is
    // true for those words, so without this guard `is_float` would read through a
    // garbage (in release, NULL) `boxed_val` pointer — a crash, not a comparison.
    // The C has no non-term behaviour here at all (it dereferences too), so this
    // is a documented, panic-free hardening of the crate's own contract: **never
    // a wrong result for a valid input, never a dereference for a non-term**.
    //
    // Documented ordering for a non-term:
    // * against a well-formed term it sorts **after** every term (so it never
    //   compares `Equal` to one — a caller cannot mistake it for a real term);
    // * against another non-term it orders by the raw word, which keeps the
    //   relation a total order and makes `Equal` mean "identical word" (the fast
    //   path above), so the `eq ⇒ equal hash` consistency rule still holds.
    // Both `cmp` (exact) and `cmp_arith` (arithmetic) share this rule, so a stray
    // non-term is treated identically by `=:=` and `==`.
    let (a_term, b_term) = (is_term_word(a), is_term_word(b));
    if !(a_term && b_term) {
        return if a_term {
            Ordering::Less
        } else if b_term {
            Ordering::Greater
        } else {
            a.raw().cmp(&b.raw())
        };
    }
    if is_atom(a) && is_atom(b) {
        return cmp_atoms(a, b, atoms);
    }
    if is_small(a) && is_small(b) {
        return signed_value(a).cmp(&signed_value(b));
    }
    // SAFETY: the guard above established that both words are terms, so `is_float`
    // and `class` only dereference the header word of a live boxed term; every term
    // here was produced by this crate.
    let (a_float, b_float) = unsafe { (is_float(a), is_float(b)) };
    if a_float && b_float {
        // SAFETY: as above.
        return unsafe { cmp_floats(a, b, exact) };
    }
    let (ca, cb) = unsafe { (class(a), class(b)) };
    if is_number_class(ca) && is_number_class(cb) {
        return cmp_numbers(a, b, ca, cb, exact);
    }
    let (ra, rb) = (class_rank(ca), class_rank(cb));
    if ra != rb {
        return ra.cmp(&rb);
    }
    // Same non-numeric class: compare within the class.
    match ca {
        TermClass::Atom => cmp_atoms(a, b, atoms),
        // SAFETY: both operands are live terms of the same class.
        TermClass::Tuple => unsafe { cmp_tuples(a, b, atoms, exact) },
        TermClass::List => unsafe { cmp_lists(a, b, atoms, exact) },
        TermClass::Bitstring => unsafe { cmp_bitstrings(a, b) },
        TermClass::Pid => internal_pid_data(a).cmp(&internal_pid_data(b)),
        TermClass::Port => internal_port_data(a).cmp(&internal_port_data(b)),
        TermClass::Ref => unsafe { cmp_refs(a, b) },
        TermClass::Nil => Ordering::Equal,
        // SAFETY: both operands are live maps.
        TermClass::Map => cmp_maps(a, b, atoms, exact),
        // SAFETY: both operands are live funs.
        TermClass::Fun => unsafe { cmp_funs(a, b, atoms, exact) },
        // SAFETY: both operands are live external pids/ports/refs, or live native
        // records — one class per arm, as established by `class_rank` above.
        TermClass::ExternalPid => unsafe { cmp_external_pids(a, b, atoms, exact) },
        TermClass::ExternalPort => unsafe { cmp_external_ports(a, b, atoms, exact) },
        TermClass::ExternalRef => unsafe { cmp_external_refs(a, b, atoms, exact) },
        TermClass::Record => unsafe { cmp_records(a, b, atoms, exact) },
        // The on-heap bin ref still uses the documented deterministic fallback (see
        // the module `Known Issues`); the catch-all keeps the match total for any
        // class added later.
        _ => fallback_cmp(a, b),
    }
}

/// The *internal* flat-map key order — `erts_cmp_flatmap_keys`
/// (`erts/emulator/beam/erl_utils.h:267-272`).
///
/// Two atom keys are ordered by **atom index** (`key_a - key_b` in the C); any
/// other pair is ordered by the exact term order ([`cmp`]). This is the order in
/// which a flat map stores its keys (`erts_validate_and_sort_flatmap`) and the
/// order the C uses to compare map keys, so it — not the plain term order — is
/// what [`cmp_maps`] must use to be observable-compatible.
pub fn cmp_flatmap_keys(key_a: Eterm, key_b: Eterm, atoms: &AtomTable) -> Ordering {
    if is_atom(key_a) && is_atom(key_b) {
        atom_val(key_a).raw().cmp(&atom_val(key_b).raw())
    } else {
        cmp(key_a, key_b, atoms)
    }
}

/// Compare two maps — the `MAP_SUBTAG` arm of `erts_cmp_compound`
/// (`erts/emulator/beam/utils.c:1955-2118`).
///
/// Erlang map order is: **size first**, then the key/value pairs ordered by the
/// *flat-map key order* ([`cmp_flatmap_keys`]). The C compares the whole key
/// sequence before the value sequence (it pushes the value comparisons first and
/// the key comparisons last on its work stack, so keys are popped — and
/// therefore compared — first), which is exactly a lexicographic comparison of
/// `(key-list, value-list)`; we reproduce that literally.
///
/// Both operands are, by the caller invariant, well-formed maps. A malformed
/// map (only reachable if the term was forged outside this crate) falls back to
/// the deterministic [`fallback_cmp`] rather than panicking.
fn cmp_maps(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    let (pa, pb) = match (
        crate::map::map_pairs_flatmap_order(a, atoms),
        crate::map::map_pairs_flatmap_order(b, atoms),
    ) {
        (Ok(pa), Ok(pb)) => (pa, pb),
        _ => return fallback_cmp(a, b),
    };
    if pa.len() != pb.len() {
        return pa.len().cmp(&pb.len());
    }
    for i in 0..pa.len() {
        let ord = cmp_flatmap_keys(pa[i].0, pb[i].0, atoms);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    for i in 0..pa.len() {
        // SAFETY: the value sub-terms of two live, well-formed maps.
        let ord = cmp_impl(pa[i].1, pb[i].1, atoms, exact);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// `erts_cmp_atoms` — lexicographic comparison of the atom *names*.
///
/// The C compares a cached first-bytes ordinal, then `memcmp`s the names and
/// finally the lengths; that is exactly a lexicographic byte comparison of the
/// name strings (shorter prefix sorts first). When a name is unknown to `atoms`
/// we fall back to the raw atom index so the result stays deterministic.
fn cmp_atoms(a: Eterm, b: Eterm, atoms: &AtomTable) -> Ordering {
    let ia = atom_val(a);
    let ib = atom_val(b);
    match (atoms.name(ia), atoms.name(ib)) {
        (Some(na), Some(nb)) => na.as_bytes().cmp(nb.as_bytes()),
        _ => ia.raw().cmp(&ib.raw()),
    }
}

/// `erts_float_exact_sortable` — a total order over `f64` bits where
/// `-0.0 < +0.0` and NaNs get a stable position.
const fn float_exact_sortable(bits: u64) -> u64 {
    const SIGN_BIT: u64 = 1 << 63;
    if bits & SIGN_BIT != 0 {
        !bits
    } else {
        bits ^ SIGN_BIT
    }
}

/// Compare two boxed floats.
///
/// # Safety
///
/// `a` and `b` must be live boxed floats.
unsafe fn cmp_floats(a: Eterm, b: Eterm, exact: bool) -> Ordering {
    // SAFETY: the caller guarantees both are floats.
    let (ba, bb) = unsafe { (float_bits(a), float_bits(b)) };
    if exact {
        float_exact_sortable(ba).cmp(&float_exact_sortable(bb))
    } else {
        // Arithmetic mode: plain IEEE ordering. `partial_cmp` is `None` only when
        // a NaN is involved; `erts_float_comp` returns 1 (a > b) for that case, so
        // we mirror it to stay deterministic.
        f64::from_bits(ba)
            .partial_cmp(&f64::from_bits(bb))
            .unwrap_or(Ordering::Greater)
    }
}

/// Compare two *different-representation* numeric terms.
fn cmp_numbers(a: Eterm, b: Eterm, ca: TermClass, cb: TermClass, exact: bool) -> Ordering {
    use TermClass::{Big, Float, Small};
    match (ca, cb) {
        // A bignum is always outside the 60-bit small range, so the small's
        // position is decided solely by the bignum's sign.
        (Small, Big) => {
            if big_is_negative(b) {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Big, Small) => {
            if big_is_negative(a) {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Big, Big) => unsafe { cmp_bigs(a, b) },
        // Mixed integer/float.
        (Float, Small) | (Float, Big) | (Small, Float) | (Big, Float) => {
            if exact {
                class_rank(ca).cmp(&class_rank(cb))
            } else {
                number_as_f64(a)
                    .partial_cmp(&number_as_f64(b))
                    .unwrap_or(Ordering::Greater)
            }
        }
        _ => Ordering::Equal,
    }
}

/// Whether a bignum term is negative (`big_sign` — the header's `_BIG_SIGN_BIT`).
///
/// # Safety
///
/// `x` must be a live bignum term.
fn big_is_negative(x: Eterm) -> bool {
    // SAFETY: `x` is boxed; its header word is live.
    unsafe { (*boxed_val(x)).raw() & BIG_SIGN_BIT != 0 }
}

/// Compare two bignums limb-by-limb (`big_comp`).
///
/// Bignums store `ErtsDigit` (one machine word) limbs least-significant first,
/// normalised so the most significant limb is non-zero; comparing the sign, then
/// the limb count, then the limbs from the most significant downward is exact.
///
/// # Safety
///
/// `a` and `b` must be live bignum terms.
unsafe fn cmp_bigs(a: Eterm, b: Eterm) -> Ordering {
    let (na, nb) = (big_is_negative(a), big_is_negative(b));
    if na != nb {
        return if na { Ordering::Less } else { Ordering::Greater };
    }
    // SAFETY: both are boxed bignums.
    let (pa, pb) = (boxed_val(a), boxed_val(b));
    let (arity_a, arity_b) = unsafe { (header_arity(*pa), header_arity(*pb)) };
    let magnitude = if arity_a != arity_b {
        arity_a.cmp(&arity_b)
    } else {
        let mut ord = Ordering::Equal;
        let mut i = arity_a;
        while i > 0 {
            i -= 1;
            // SAFETY: limb `i` is within the bignum's payload.
            let (da, db) = unsafe { ((*pa.add(1 + i)).raw(), (*pb.add(1 + i)).raw()) };
            if da != db {
                ord = da.cmp(&db);
                break;
            }
        }
        ord
    };
    if na {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// Compare two tuples: by arity first, then elementwise.
///
/// # Safety
///
/// `a` and `b` must be live tuple terms.
unsafe fn cmp_tuples(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    // SAFETY: both are tuples.
    let (sa, sb) = unsafe { (tuple_size(a), tuple_size(b)) };
    if sa != sb {
        return sa.cmp(&sb);
    }
    for i in 0..sa {
        // SAFETY: `i < sa`, the tuple's arity.
        let (ea, eb) = unsafe { (tuple_element(a, i), tuple_element(b, i)) };
        let ord = cmp_impl(ea, eb, atoms, exact);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Compare two cons cells: elementwise, with the (possibly improper) tail
/// deciding a prefix tie (shorter/`nil` sorts first).
///
/// # Safety
///
/// `a` and `b` must be live non-`NIL` cons cells.
unsafe fn cmp_lists(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    let mut x = a;
    let mut y = b;
    loop {
        // SAFETY: `x` and `y` are cons cells.
        let (ca, cb) = unsafe { (car(x), car(y)) };
        if ca.raw() != cb.raw() {
            let ord = cmp_impl(ca, cb, atoms, exact);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        // SAFETY: `x` and `y` are cons cells.
        let (da, db) = unsafe { (cdr(x), cdr(y)) };
        if da.raw() == db.raw() {
            return Ordering::Equal;
        }
        if !is_list(da) || !is_list(db) {
            // At least one tail is `nil` or an improper tail: the general
            // comparison decides (e.g. `nil` < cons).
            return cmp_impl(da, db, atoms, exact);
        }
        x = da;
        y = db;
    }
}

/// `erts_cmp_bits` — compare two bitstrings bit-by-bit (most-significant first),
/// then by bit size.
///
/// # Safety
///
/// `a` and `b` must be live bitstring terms.
unsafe fn cmp_bitstrings(a: Eterm, b: Eterm) -> Ordering {
    // SAFETY: both are bitstrings.
    let asz = unsafe { bitstring_parts(a) }.2;
    // SAFETY: as above.
    let bsz = unsafe { bitstring_parts(b) }.2;
    let n = asz.min(bsz);
    let mut i = 0;
    while i < n {
        // SAFETY: bit `i` is within both bitstrings (`i < n <= asz, bsz`).
        let abit = unsafe { bit_at(a, i) };
        // SAFETY: as above.
        let bbit = unsafe { bit_at(b, i) };
        if abit != bbit {
            return abit.cmp(&bbit);
        }
        i += 1;
    }
    asz.cmp(&bsz)
}

/// Compare two internal references by their raw payload words, most significant
/// first. See the module `Known Issues`.
///
/// # Safety
///
/// `a` and `b` must be live internal reference terms.
unsafe fn cmp_refs(a: Eterm, b: Eterm) -> Ordering {
    // SAFETY: both are boxed refs.
    let (pa, pb) = (boxed_val(a), boxed_val(b));
    let (arity_a, arity_b) = unsafe { (header_arity(*pa), header_arity(*pb)) };
    if arity_a != arity_b {
        return arity_a.cmp(&arity_b);
    }
    let mut i = arity_a;
    while i > 0 {
        i -= 1;
        // SAFETY: word `i` is within the ref thing's payload.
        let (wa, wb) = unsafe { ((*pa.add(1 + i)).raw(), (*pb.add(1 + i)).raw()) };
        if wa != wb {
            return wa.cmp(&wb);
        }
    }
    Ordering::Equal
}

/// `CMP_NODES` (`erts/emulator/beam/utils.c:1715-1723`) for the M1 node
/// representation: order two nodes by their sysname atom, then by `creation`.
///
/// The C short-circuits on node *pointer* identity and otherwise compares
/// `(AN)->sysname` with `erts_cmp_atoms` followed by `(AN)->creation`. This tree has
/// no node registry — the sysname atom and the creation live *inside* the term (the
/// M1 substitution documented on [`crate::thing::write_external_pid`]) — so there is
/// no pointer identity to short-circuit on and both fields are always compared, in
/// that order.
fn cmp_nodes(
    node_a: Eterm,
    creation_a: u32,
    node_b: Eterm,
    creation_b: u32,
    atoms: &AtomTable,
    exact: bool,
) -> Ordering {
    let ord = cmp_impl(node_a, node_b, atoms, exact);
    if ord != Ordering::Equal {
        return ord;
    }
    creation_a.cmp(&creation_b)
}

/// Compare two external pids — the `pid_common` block of `erts_cmp_compound`
/// (`erts/emulator/beam/utils.c:1777-1811`).
///
/// Field order (`utils.c:1805-1809`): `serial`, then `number`, then the node
/// (sysname atom, then `creation`). Both operands are external, so the C's
/// internal/external mixed branches do not apply.
///
/// # Safety
///
/// `a` and `b` must be live external pid terms.
unsafe fn cmp_external_pids(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    // SAFETY: the caller guarantees both operands are live external pids written by
    // `crate::thing::write_external_pid`.
    let (node_a, number_a, serial_a, creation_a) = unsafe { external_pid_parts(a) };
    // SAFETY: as above.
    let (node_b, number_b, serial_b, creation_b) = unsafe { external_pid_parts(b) };
    serial_a
        .cmp(&serial_b)
        .then_with(|| number_a.cmp(&number_b))
        .then_with(|| cmp_nodes(node_a, creation_a, node_b, creation_b, atoms, exact))
}

/// Compare two external ports — the `_TAG_HEADER_EXTERNAL_PORT` arm of
/// `erts_cmp_compound` (`erts/emulator/beam/utils.c:2219-2238`).
///
/// Field order: the node (sysname atom, then `creation`), then `number` (the 64-bit
/// `V4_PORT_EXT` id, `external_port_number`).
///
/// # Safety
///
/// `a` and `b` must be live external port terms.
unsafe fn cmp_external_ports(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    // SAFETY: the caller guarantees both operands are live external ports written by
    // `crate::thing::write_external_port`.
    let (node_a, number_a, creation_a) = unsafe { external_port_parts(a) };
    // SAFETY: as above.
    let (node_b, number_b, creation_b) = unsafe { external_port_parts(b) };
    cmp_nodes(node_a, creation_a, node_b, creation_b, atoms, exact)
        .then_with(|| number_a.cmp(&number_b))
}

/// Compare two external references — the `_TAG_HEADER_EXTERNAL_REF` arm of
/// `erts_cmp_compound` and its `ref_common` tail
/// (`erts/emulator/beam/utils.c:2239-2308`).
///
/// Field order: the node (sysname atom, then `creation`, `utils.c:2263`), then the
/// ids as one big integer (`utils.c:2265-2287`) — index `len - 1` is the most
/// significant. When the two id slices differ in length, the longer one's leading
/// (highest-index) **zero** ids are trimmed first; a non-zero leading id instead
/// decides immediately (the longer slice is the greater one). The surviving ids are
/// then compared from the highest index down.
///
/// # Safety
///
/// `a` and `b` must be live external reference terms.
unsafe fn cmp_external_refs(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    // SAFETY: the caller guarantees both operands are live external references
    // written by `crate::thing::write_external_ref`.
    let (node_a, ids_a, creation_a) = unsafe { external_ref_parts(a) };
    // SAFETY: as above.
    let (node_b, ids_b, creation_b) = unsafe { external_ref_parts(b) };
    let ord = cmp_nodes(node_a, creation_a, node_b, creation_b, atoms, exact);
    if ord != Ordering::Equal {
        return ord;
    }
    // Trim the longer slice's leading zero ids — the C's `do { … } while (alen >
    // blen)` loops reproduced literally, so no index can go out of bounds. The loops
    // exit with equal lengths (`ASSERT(alen == blen)`, `utils.c:2283`).
    let (mut la, mut lb) = (ids_a.len(), ids_b.len());
    if la > lb {
        loop {
            if ids_a[la - 1] != 0 {
                return Ordering::Greater;
            }
            la -= 1;
            if la <= lb {
                break;
            }
        }
    } else if lb > la {
        loop {
            if ids_b[lb - 1] != 0 {
                return Ordering::Less;
            }
            lb -= 1;
            if la >= lb {
                break;
            }
        }
    }
    debug_assert_eq!(la, lb);
    // Compare the surviving ids from the most significant (highest index) down.
    let mut i = la.min(lb);
    while i > 0 {
        i -= 1;
        if ids_a[i] != ids_b[i] {
            return ids_a[i].cmp(&ids_b[i]);
        }
    }
    Ordering::Equal
}

/// The comparable words of a native-record **instance**, as read through the
/// [`crate::record`] accessors: the field count, the definition's
/// `(module, name, is_exported)` words, and its *sorted-key-ordered* key and value
/// slices.
struct RecordParts<'a> {
    field_count: usize,
    module: Eterm,
    name: Eterm,
    is_exported: Eterm,
    keys: &'a [Eterm],
    values: &'a [Eterm],
}

/// Reads a native-record instance's comparable words.
///
/// `None` for a malformed record — only reachable if the term was forged outside
/// this crate (a definition that is not a valid definition block, or one whose arity
/// disagrees with the instance's own header, as [`crate::record::record_get`] also
/// guards against).
///
/// # Safety
///
/// `t` must be a live native-record instance; its storage must stay live and
/// unmoved for `'a`.
unsafe fn record_parts<'a>(t: Eterm) -> Option<RecordParts<'a>> {
    // SAFETY: the caller guarantees `t` is a live native record, so its definition
    // block, its key array and its own value words are all live.
    let (field_count, keys, values, module, name, is_exported) = unsafe {
        let field_count = record_field_count(t).ok()?;
        let def = record_definition(t).ok()?;
        (
            field_count,
            record_definition_keys(def).ok()?,
            record_values(t).ok()?,
            record_definition_module(def).ok()?,
            record_definition_name(def).ok()?,
            record_definition_is_exported(def).ok()?,
        )
    };
    if keys.len() != field_count || values.len() != field_count {
        return None;
    }
    Some(RecordParts {
        field_count,
        module,
        name,
        is_exported,
        keys,
        values,
    })
}

/// The `false < true` order of a record definition's `is_exported` word
/// (`erts/emulator/beam/utils.c:1917-1921`, `ASSERT(false < true)`).
///
/// The C stores the flag as an `int` (`0`/`1`) and subtracts the two operands; this
/// crate stores the atom `false`/`true` (`AM_FALSE`/`AM_TRUE`, atom indices 0/1, see
/// [`crate::record`]), so the atom index is the comparable value. A word that is not
/// an atom (only reachable for a forged record) ranks after both.
fn exported_rank(is_exported: Eterm) -> u32 {
    if is_atom(is_exported) {
        atom_val(is_exported).raw()
    } else {
        u32::MAX
    }
}

/// Compare two native records — the `_TAG_HEADER_RECORD` arm of `erts_cmp_compound`
/// (`erts/emulator/beam/utils.c:1878-1954`).
///
/// The field order: the definition's `module` atom, `name` atom (both through
/// [`cmp_impl`], which routes two atoms to the C's `erts_cmp_atoms`) and
/// `is_exported` (`false < true`), then the instance's field
/// count, then the field **keys**, and finally the field **values** (`utils.c:1941-1952`;
/// the C pushes the value pairs in reverse so they pop — and are compared — in
/// ascending order).
///
/// This crate stores the definition keys and the instance values in **sorted-key
/// order** (`crate::record`), so keys and values are compared index-aligned, and the
/// comparison is always structural. **Documented deviation from the C:** `def_a !=
/// def_b` there is a *canonicalisation pointer short-circuit* for the common "same
/// definition" case, and its key/value loops index through `order_a[i]`/`order_b[i]`
/// (declaration order) with a `RECORD_VALUE_PAIR` work-stack permutation. For two
/// records with the same definition the result is exactly the C's; for the exotic
/// case of two records that declare the same fields in *different* orders the C
/// pairs them by declared position while this comparison pairs them by sorted key,
/// so it can differ. (The field-count check uses [`record_field_count`] where the C
/// compares `record_header_arity` — the same value `+ 1`, so the order is identical.)
///
/// A malformed record (only reachable if the term was forged outside this crate)
/// falls back to the deterministic [`fallback_cmp`] rather than panicking.
///
/// # Safety
///
/// `a` and `b` must be live native record terms.
unsafe fn cmp_records(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    // SAFETY: the caller guarantees both operands are live native records.
    let (parts_a, parts_b) = unsafe { (record_parts(a), record_parts(b)) };
    let (Some(ra), Some(rb)) = (parts_a, parts_b) else {
        return fallback_cmp(a, b);
    };
    // The definition content, in the C's order (`utils.c:1907-1938`).
    let ord = cmp_impl(ra.module, rb.module, atoms, exact);
    if ord != Ordering::Equal {
        return ord;
    }
    let ord = cmp_impl(ra.name, rb.name, atoms, exact);
    if ord != Ordering::Equal {
        return ord;
    }
    let ord = exported_rank(ra.is_exported).cmp(&exported_rank(rb.is_exported));
    if ord != Ordering::Equal {
        return ord;
    }
    let ord = ra.field_count.cmp(&rb.field_count);
    if ord != Ordering::Equal {
        return ord;
    }
    for i in 0..ra.field_count {
        let ord = cmp_impl(ra.keys[i], rb.keys[i], atoms, exact);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    for i in 0..ra.field_count {
        let ord = cmp_impl(ra.values[i], rb.values[i], atoms, exact);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// The ascending kind rank of a fun, used as the leading key when two funs are
/// of different kinds.
///
/// C orders `External` after any local fun (`utils.c:2207-2209`). C has only one
/// local kind (its `NEW_FUN_EXT` fun is an ordinary `ErlFunThing` with an
/// `ErlFunEntry` entry), so this crate additionally orders the M1
/// [`FunKind::Local`] descriptor before the [`FunKind::Imported`] one — see
/// [`cmp_funs`].
const fn fun_kind_rank(kind: FunKind) -> u8 {
    match kind {
        FunKind::Local => 0,
        FunKind::Imported => 1,
        FunKind::External => 2,
    }
}

/// Compare two funs — the `_TAG_HEADER_FUN` arm of `erts_cmp_compound`
/// (`erts/emulator/beam/utils.c:2149-2210`).
///
/// The C compares the two funs' out-of-line entries:
///
/// * two local funs compare `(entry.module, entry.index, entry.old_uniq)`, then
///   `num_free`, then the environment element-wise;
/// * two external funs compare `(mfa.module, mfa.function, mfa.arity)`;
/// * an external fun sorts after any local one.
///
/// This crate inlines the entry into the term (see [`crate::fun`]), so the
/// per-kind keys are:
///
/// * [`FunKind::Local`] ⇒ `(uniq, index)`, then `num_free`, then the environment.
///   **Documented deviation:** the M1 `Local` descriptor carries `uniq`/`index`
///   only, so the C's `module`/`old_uniq` components are not available; `uniq` is
///   the fun's identity, so it leads. The C's `old_uniq` has no counterpart here.
/// * [`FunKind::Imported`] ⇒ `(module, index, old_uniq)`, then `num_free`, then
///   the environment — exactly the C's field order (`index`/`old_uniq` compared
///   unsigned; the C subtracts them as `int`s, which is unobservable for real
///   lambda indices).
/// * `External` ⇒ `(module, function, arity)`.
///
/// **Documented deviation for mixed `Local`/`Imported` pairs:** their descriptors
/// have different shapes, so — *as far as the descriptor allows* — they are
/// ordered by kind (`Local` first). Ordering them by `index` instead (the one
/// field both carry) would make the relation intransitive (a `Local`, an
/// `Imported` and another `Local` could each sort below the next in a cycle), so
/// the kind is the leading key.
///
/// Arity is **not** compared for the two local kinds, mirroring the C: there the
/// arity lives in the header but is pinned by the entry, so the C never compares
/// it for a local fun. (For an external fun the C compares `mfa.arity`
/// explicitly, and so does this function.)
///
/// # Safety
///
/// `a` and `b` must be live fun terms.
unsafe fn cmp_funs(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    let (pa, pb) = (boxed_val(a), boxed_val(b));
    // SAFETY: the caller guarantees both operands are live funs, so word 0 of each
    // is its (live) header word.
    let (ha, hb) = unsafe { (*pa, *pb) };
    let (ka, kb) = (fun_kind_of_header(ha), fun_kind_of_header(hb));
    if ka != kb {
        return fun_kind_rank(ka).cmp(&fun_kind_rank(kb));
    }
    match ka {
        // The C's `Export` is canonical, so `(module, function, arity)` decides.
        FunKind::External => {
            // SAFETY: an external fun's descriptor words 1 and 2 hold its module and
            // function atoms.
            let ord = cmp_impl(unsafe { *pa.add(1) }, unsafe { *pb.add(1) }, atoms, exact);
            if ord != Ordering::Equal {
                return ord;
            }
            // SAFETY: as above — descriptor word 2.
            let ord = cmp_impl(unsafe { *pa.add(2) }, unsafe { *pb.add(2) }, atoms, exact);
            if ord != Ordering::Equal {
                return ord;
            }
            fun_arity(ha).cmp(&fun_arity(hb))
        }
        // The inlined `(uniq, index)` descriptor stands in for the C entry.
        FunKind::Local => {
            // SAFETY: a local fun's descriptor words 1 and 2 hold `uniq` and `index`.
            let (ua, ub) = unsafe { ((*pa.add(1)).raw(), (*pb.add(1)).raw()) };
            let ord = ua.cmp(&ub);
            if ord != Ordering::Equal {
                return ord;
            }
            // SAFETY: as above — descriptor word 2.
            let (ia, ib) = unsafe { (((*pa.add(2)).raw() as u32), ((*pb.add(2)).raw() as u32)) };
            let ord = ia.cmp(&ib);
            if ord != Ordering::Equal {
                return ord;
            }
            // SAFETY: both operands are live local-kind funs (see the fn docs).
            unsafe { cmp_fun_env(pa, pb, ha, hb, atoms, exact) }
        }
        // The C's local-fun entry fields the wire format also carries.
        FunKind::Imported => {
            // SAFETY: an imported fun's descriptor word 1 holds its module atom.
            let ord = cmp_impl(unsafe { *pa.add(1) }, unsafe { *pb.add(1) }, atoms, exact);
            if ord != Ordering::Equal {
                return ord;
            }
            // SAFETY: an imported fun's descriptor word 4 holds `index`.
            let (ia, ib) = unsafe { (((*pa.add(4)).raw() as u32), ((*pb.add(4)).raw() as u32)) };
            let ord = ia.cmp(&ib);
            if ord != Ordering::Equal {
                return ord;
            }
            // SAFETY: an imported fun's descriptor word 6 holds `old_uniq`.
            let (oa, ob) = unsafe { (((*pa.add(6)).raw() as u32), ((*pb.add(6)).raw() as u32)) };
            let ord = oa.cmp(&ob);
            if ord != Ordering::Equal {
                return ord;
            }
            // SAFETY: both operands are live local-kind funs (see the fn docs).
            unsafe { cmp_fun_env(pa, pb, ha, hb, atoms, exact) }
        }
    }
}

/// Compare two local-kind funs' environments: `num_free` first, then the
/// `num_free` elements (the C's `term_array`, `utils.c:2181-2191`).
///
/// # Safety
///
/// `pa`/`pb` must be the header words of live local-kind funs `ha`/`hb`, and both
/// funs' blocks must be live.
unsafe fn cmp_fun_env(
    pa: *mut Eterm,
    pb: *mut Eterm,
    ha: Eterm,
    hb: Eterm,
    atoms: &AtomTable,
    exact: bool,
) -> Ordering {
    let (na, nb) = (fun_num_free(ha), fun_num_free(hb));
    let ord = na.cmp(&nb);
    if ord != Ordering::Equal {
        return ord;
    }
    let (oa, ob) = (
        1 + fun_desc_words(fun_kind_of_header(ha)),
        1 + fun_desc_words(fun_kind_of_header(hb)),
    );
    for i in 0..na {
        // SAFETY: environment word `i` of each live fun (both have `na` of them).
        let (ea, eb) = unsafe { (*pa.add(oa + i), *pb.add(ob + i)) };
        let ord = cmp_impl(ea, eb, atoms, exact);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// The documented deterministic fallback for classes without a faithful
/// layout-aware comparison: order by object arity, then by the raw term word.
///
/// This never panics. See the module `Known Issues` for the affected classes.
fn fallback_cmp(a: Eterm, b: Eterm) -> Ordering {
    // SAFETY: both terms are boxed (this is only reached for boxed classes).
    let (arity_a, arity_b) = unsafe { (header_arity(*boxed_val(a)), header_arity(*boxed_val(b))) };
    arity_a
        .cmp(&arity_b)
        .then_with(|| a.raw().cmp(&b.raw()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{make_boxed, make_header, HEADER_FLONUM};
    use crate::fun::{fun_words, write_fun_external, write_fun_imported, write_fun_local, FunPid};
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data, make_small, NIL};
    use crate::tag::{HAMT_SUBTAG_HEAD_FLATMAP, HEAP_BITS_SUBTAG, REF_SUBTAG};
    use crate::Atom;

    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "b", "c", "z"]);
        t
    }

    /// Build a [`FunKind::Local`] fun with `env` as its free variables.
    ///
    /// # Safety
    ///
    /// `heap` must own the [`fun_words`]-sized block and stay alive (and unmoved)
    /// for as long as the returned term is used.
    unsafe fn local_fun(heap: &mut Heap, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
        // SAFETY: the caller keeps the block alive; `write_fun_local` writes it all.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::Local, env.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(Local, env.len())` words.
        unsafe { write_fun_local(hp, 0, uniq, index, env).unwrap() }
    }

    /// Build a [`FunKind::External`] fun.
    ///
    /// # Safety
    ///
    /// As [`local_fun`].
    unsafe fn external_fun(heap: &mut Heap, module: Atom, function: Atom, arity: usize) -> Eterm {
        // SAFETY: the caller keeps the block alive.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::External, 0)).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(External, 0)` words.
        unsafe { write_fun_external(hp, arity, module, function).unwrap() }
    }

    /// Build a [`FunKind::Imported`] fun with `env` as its free variables.
    ///
    /// # Safety
    ///
    /// As [`local_fun`].
    unsafe fn imported_fun(
        heap: &mut Heap,
        module: Atom,
        index: u32,
        old_uniq: u32,
        env: &[Eterm],
    ) -> Eterm {
        // SAFETY: the caller keeps the block alive.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::Imported, env.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(Imported, env.len())` words.
        unsafe {
            write_fun_imported(
                hp,
                0,
                module,
                &[0u8; 16],
                index,
                0,
                old_uniq,
                FunPid {
                    node: module,
                    number: 0,
                    serial: 0,
                    creation: 0,
                },
                env,
            )
            .unwrap()
        }
    }

    /// Build `#{Atom => Value}` as a flatmap (see `erl_map.h`).
    unsafe fn flatmap(heap: &mut Heap, key: Eterm, value: Eterm) -> Eterm {
        let keys = heap.tuple(&[key]).unwrap();
        // SAFETY: the 4 words are fully written before the map term escapes.
        let p = unsafe { heap.alloc_words(4).unwrap() };
        unsafe {
            *p = make_header(0, HAMT_SUBTAG_HEAD_FLATMAP);
            *p.add(1) = Eterm::from_raw(1); // flatmap size
            *p.add(2) = keys;
            *p.add(3) = value;
        }
        make_boxed(p)
    }

    /// Build an internal ordinary ref thing (`ErtsORefThing`).
    unsafe fn internal_ref(heap: &mut Heap) -> Eterm {
        let p = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *p = make_header(2, REF_SUBTAG);
            *p.add(1) = Eterm::from_raw(0xffff_ffff_0000_0000);
            *p.add(2) = Eterm::from_raw(0x0000_0002_0000_0001);
        }
        make_boxed(p)
    }

    /// Build a 16-bit (2-byte) heap binary from a `u16`.
    unsafe fn heap_binary(heap: &mut Heap, value: u16) -> Eterm {
        let p = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *p = make_header(2, HEAP_BITS_SUBTAG);
            *p.add(1) = Eterm::from_raw(16); // bit size
            *p.add(2) = Eterm::from_raw(value as usize);
        }
        make_boxed(p)
    }

    fn float_term(bits: f64) -> Eterm {
        let words: Box<[Eterm; 2]> = Box::new([HEADER_FLONUM, Eterm::from_raw(bits.to_bits() as usize)]);
        let leaked: &'static [Eterm; 2] = Box::leak(words);
        make_boxed(leaked.as_ptr())
    }

    #[test]
    fn ordering_matches_the_documented_class_order() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let atom = make_atom(a.lookup("a").unwrap());
        // SAFETY: the heap owns every boxed word built here.
        let (tup, map, list, bin, r, pid, port) = unsafe {
            (
                heap.tuple(&[make_small(1)]).unwrap(),
                flatmap(&mut heap, make_small(1), make_small(2)),
                heap.cons(make_small(1), NIL).unwrap(),
                heap_binary(&mut heap, 0x0102),
                internal_ref(&mut heap),
                make_internal_pid(make_pid_data(0, 7)),
                make_internal_port(make_port_data(7)),
            )
        };
        let number = make_small(42);

        // number < atom < ref < port < pid < tuple < map < nil < list < binary
        let ascending = [
            number, atom, r, port, pid, tup, map, NIL, list, bin,
        ];
        for i in 0..ascending.len() {
            for j in 0..ascending.len() {
                let ord = cmp(ascending[i], ascending[j], &a);
                if i < j {
                    assert_eq!(ord, Ordering::Less, "expected {i} < {j}: {ord:?}");
                } else if i == j {
                    assert_eq!(ord, Ordering::Equal, "expected {i} == {j}");
                } else {
                    assert_eq!(ord, Ordering::Greater, "expected {i} > {j}");
                }
            }
        }
    }

    #[test]
    fn atoms_compare_by_name_not_index() {
        let a = atoms();
        let (ia, ib, iz) = (
            make_atom(a.lookup("a").unwrap()),
            make_atom(a.lookup("b").unwrap()),
            make_atom(a.lookup("z").unwrap()),
        );
        assert_eq!(cmp(ia, ib, &a), Ordering::Less);
        assert_eq!(cmp(ib, ia, &a), Ordering::Greater);
        assert_eq!(cmp(ia, ia, &a), Ordering::Equal);
        assert_eq!(cmp(iz, ia, &a), Ordering::Greater);
        assert!(eq(ia, ia, &a));
        assert!(!eq(ia, ib, &a));
    }

    #[test]
    fn unknown_atom_name_falls_back_to_index() {
        let empty = AtomTable::new();
        let a0 = make_atom(crate::Atom::from_raw(0));
        let a1 = make_atom(crate::Atom::from_raw(1));
        // No names known: deterministic index comparison, never a panic.
        assert_eq!(cmp(a0, a1, &empty), Ordering::Less);
        assert_eq!(cmp(a1, a0, &empty), Ordering::Greater);
        assert_eq!(cmp(a0, a0, &empty), Ordering::Equal);
    }

    #[test]
    fn small_integers_compare_numerically() {
        let a = atoms();
        for (x, y) in [(-5i64, 5i64), (0, 1), (-100, -99), (1_000_000, -1_000_000)] {
            assert_eq!(cmp(make_small(x), make_small(y), &a), x.cmp(&y));
        }
    }

    #[test]
    fn tuples_compare_elementwise_then_by_arity() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let t1 = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let t2 = heap.tuple(&[make_small(1), make_small(3)]).unwrap();
        let t3 = heap.tuple(&[make_small(1), make_small(2), make_small(0)]).unwrap();
        let t4 = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        assert_eq!(cmp(t1, t2, &a), Ordering::Less); // second element
        assert_eq!(cmp(t2, t1, &a), Ordering::Greater);
        assert_eq!(cmp(t1, t4, &a), Ordering::Equal); // structurally equal
        assert_eq!(cmp(t1, t3, &a), Ordering::Less); // shorter arity sorts first
        assert!(eq(t1, t4, &a));
    }

    #[test]
    fn lists_compare_elementwise_then_by_tail() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let nil_list = NIL;
        let one = heap.cons(make_small(1), NIL).unwrap();
        let two = heap.cons(make_small(2), NIL).unwrap();
        let two_copy = heap.cons(make_small(2), NIL).unwrap();
        let one_two = heap.cons(make_small(1), two_copy).unwrap();
        let one_copy = heap.cons(make_small(1), NIL).unwrap();
        assert_eq!(cmp(one, two, &a), Ordering::Less);
        assert_eq!(cmp(one, one_copy, &a), Ordering::Equal);
        // `[1]` is a prefix of `[1,2]` => shorter sorts first.
        assert_eq!(cmp(one, one_two, &a), Ordering::Less);
        assert_eq!(cmp(one_two, one, &a), Ordering::Greater);
        // `[]` < `[..]`
        assert_eq!(cmp(nil_list, one, &a), Ordering::Less);
    }

    #[test]
    fn binaries_compare_bitwise_then_by_size() {
        let a = atoms();
        let mut heap = Heap::new(64);
        // SAFETY: heap owns these binaries.
        let (b1, b2, b3) = unsafe {
            (
                heap_binary(&mut heap, 0x0102),
                heap_binary(&mut heap, 0x0103),
                heap_binary(&mut heap, 0x0102),
            )
        };
        assert_eq!(cmp(b1, b2, &a), Ordering::Less);
        assert_eq!(cmp(b2, b1, &a), Ordering::Greater);
        assert_eq!(cmp(b1, b3, &a), Ordering::Equal);
    }

    #[test]
    fn internal_pids_and_ports_compare_by_payload() {
        let a = atoms();
        let p1 = make_internal_pid(make_pid_data(0, 1));
        let p2 = make_internal_pid(make_pid_data(0, 2));
        assert_eq!(cmp(p1, p2, &a), Ordering::Less);
        let o1 = make_internal_port(make_port_data(10));
        let o2 = make_internal_port(make_port_data(20));
        assert_eq!(cmp(o1, o2, &a), Ordering::Less);
        assert_eq!(cmp(o1, o1, &a), Ordering::Equal);
    }

    #[test]
    fn floats_use_exact_total_order_in_exact_mode() {
        let a = atoms();
        let zero = float_term(0.0);
        let neg_zero = float_term(-0.0);
        let one = float_term(1.0);
        // exact (`=:=`): -0.0 < +0.0
        assert_eq!(cmp(neg_zero, zero, &a), Ordering::Less);
        assert_eq!(cmp(one, zero, &a), Ordering::Greater);
        assert!(!eq(neg_zero, zero, &a));
        // arithmetic (`==`): -0.0 == +0.0
        assert_eq!(cmp_arith(neg_zero, zero, &a), Ordering::Equal);
        assert_eq!(cmp_arith(float_term(1.5), float_term(1.25), &a), Ordering::Greater);
    }

    #[test]
    fn integer_float_exactness_matches_erts_cmp() {
        let a = atoms();
        let one = make_small(1);
        let one_f = float_term(1.0);
        // exact (`=:=`): different classes order by class rank => number < float,
        // but small vs float is *not* numeric equality.
        assert!(!eq(one, one_f, &a));
        assert_eq!(cmp(one, one_f, &a), Ordering::Less);
        // arithmetic (`==`): numeric comparison => equal.
        assert_eq!(cmp_arith(one, one_f, &a), Ordering::Equal);
    }

    #[test]
    fn local_funs_compare_by_uniq_then_index_then_env() {
        let a = atoms();
        let mut heap = Heap::new(128);
        // SAFETY: the heap owns every block built here.
        let (l1, l2, l3, l4, l5, l1_copy) = unsafe {
            (
                local_fun(&mut heap, 1, 2, &[]),
                local_fun(&mut heap, 5, 2, &[]),
                local_fun(&mut heap, 1, 9, &[]),
                local_fun(&mut heap, 1, 2, &[make_small(0)]),
                local_fun(&mut heap, 1, 2, &[make_small(1)]),
                local_fun(&mut heap, 1, 2, &[]),
            )
        };
        assert_eq!(cmp(l1, l2, &a), Ordering::Less, "uniq decides");
        assert_eq!(cmp(l1, l3, &a), Ordering::Less, "index decides");
        assert_eq!(cmp(l1, l4, &a), Ordering::Less, "fewer free vars first");
        assert_eq!(cmp(l4, l5, &a), Ordering::Less, "environment element-wise");
        assert_eq!(cmp(l1, l1_copy, &a), Ordering::Equal);
        assert!(eq(l1, l1_copy, &a));
    }

    #[test]
    fn external_funs_compare_by_mfa_and_sort_after_local_kinds() {
        let a = atoms();
        let mut heap = Heap::new(128);
        let m = a.lookup("a").unwrap();
        let n = a.lookup("b").unwrap();
        // SAFETY: the heap owns every block built here.
        let (e1, e2, e3, e1_copy, l, i) = unsafe {
            (
                external_fun(&mut heap, m, n, 2),
                external_fun(&mut heap, n, m, 2),
                external_fun(&mut heap, m, n, 3),
                external_fun(&mut heap, m, n, 2),
                local_fun(&mut heap, 0, 0, &[]),
                imported_fun(&mut heap, m, 0, 0, &[]),
            )
        };
        assert_eq!(cmp(e1, e2, &a), Ordering::Less, "module decides");
        assert_eq!(cmp(e1, e3, &a), Ordering::Less, "arity decides");
        assert_eq!(cmp(e1, e1_copy, &a), Ordering::Equal);
        assert_eq!(cmp(l, e1, &a), Ordering::Less, "external sorts after any local");
        assert_eq!(cmp(i, e1, &a), Ordering::Less);
        assert_eq!(cmp(e1, l, &a), Ordering::Greater);
    }

    #[test]
    fn imported_funs_compare_by_module_index_old_uniq_then_env() {
        let a = atoms();
        let mut heap = Heap::new(128);
        let m = a.lookup("a").unwrap();
        let n = a.lookup("b").unwrap();
        // SAFETY: the heap owns every block built here.
        let (i1, i2, i3, i4, i5) = unsafe {
            (
                imported_fun(&mut heap, m, 1, 0, &[]),
                imported_fun(&mut heap, n, 1, 0, &[]),
                imported_fun(&mut heap, m, 2, 0, &[]),
                imported_fun(&mut heap, m, 1, 7, &[]),
                imported_fun(&mut heap, m, 1, 0, &[make_small(0)]),
            )
        };
        assert_eq!(cmp(i1, i2, &a), Ordering::Less, "module decides");
        assert_eq!(cmp(i1, i3, &a), Ordering::Less, "index decides");
        assert_eq!(cmp(i1, i4, &a), Ordering::Less, "old_uniq decides");
        assert_eq!(cmp(i1, i5, &a), Ordering::Less, "num_free decides");
        // The M1 local descriptor sorts before the imported one.
        // SAFETY: the heap owns the block.
        let l = unsafe { local_fun(&mut heap, 0, 0, &[]) };
        assert_eq!(cmp(l, i1, &a), Ordering::Less);
        assert_eq!(cmp(i1, l, &a), Ordering::Greater);
    }

    #[test]
    fn fun_ordering_is_total_over_mixed_kinds() {
        // Leading with the kind is what keeps the order transitive: ordering a
        // mixed Local/Imported pair by `index` alone could cycle.
        let a = atoms();
        let mut heap = Heap::new(128);
        // SAFETY: the heap owns every block built here.
        let (l_hi, l_lo, imp) = unsafe {
            (
                local_fun(&mut heap, 9, 2, &[]),
                local_fun(&mut heap, 1, 5, &[]),
                imported_fun(&mut heap, a.lookup("a").unwrap(), 2, 0, &[]),
            )
        };
        let mut sorted = [l_hi, imp, l_lo];
        sorted.sort_by(|x, y| cmp(*x, *y, &a));
        assert_eq!(sorted, [l_lo, l_hi, imp]);
        // The relation is antisymmetric across the kind boundary too.
        assert_eq!(cmp(l_hi, imp, &a), Ordering::Less);
        assert_eq!(cmp(imp, l_hi, &a), Ordering::Greater);
    }

    /// A word that is not a term at all must never be dereferenced: it sorts after
    /// every real term (so it never ties with one) and two non-terms order by their
    /// raw word. See the module docs.
    #[test]
    fn a_non_term_is_never_dereferenced_and_sorts_after_every_term() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let samples = [
            make_small(0),
            make_small(-1),
            make_atom(a.lookup("a").unwrap()),
            NIL,
            heap.tuple(&[make_small(1)]).unwrap(),
            heap.cons(make_small(1), NIL).unwrap(),
        ];
        // The ABI's `THE_NON_VALUE`, a bare `FLOAT` header word (what a 1-word
        // "flonum" header looks like) and an arbitrary other header word.
        for nt in [
            crate::ops::THE_NON_VALUE,
            HEADER_FLONUM,
            Eterm::from_raw(0x3c),
        ] {
            assert!(!crate::tag::is_term_word(nt));
            // The identical word still compares equal (it *is* the same word)...
            assert_eq!(cmp(nt, nt, &a), Ordering::Equal);
            assert_eq!(cmp_arith(nt, nt, &a), Ordering::Equal);
            assert!(eq(nt, nt, &a));
            for t in &samples {
                // ... but a non-term never ties with, and never equals, a real term.
                assert_eq!(cmp(nt, *t, &a), Ordering::Greater, "{nt:?} vs {t:?}");
                assert_eq!(cmp(*t, nt, &a), Ordering::Less, "{t:?} vs {nt:?}");
                assert_eq!(cmp_arith(nt, *t, &a), Ordering::Greater);
                assert_eq!(cmp_arith(*t, nt, &a), Ordering::Less);
                assert!(!eq(nt, *t, &a));
                assert!(!eq(*t, nt, &a));
            }
        }
        // Two *different* non-term words order deterministically by their raw word
        // (`THE_NON_VALUE` is `0` in release and `0x18` in a debug build; both are
        // below the `0x58` bare flonum header).
        let (lo, hi) = (crate::ops::THE_NON_VALUE, HEADER_FLONUM);
        assert!(lo.raw() < hi.raw());
        assert_eq!(cmp(lo, hi, &a), Ordering::Less);
        assert_eq!(cmp(hi, lo, &a), Ordering::Greater);
    }

    /// The non-term guard must be a no-op for valid input: every well-formed term
    /// still compares by the documented term order
    /// (`number < atom < ... < tuple < ... < nil < list`).
    #[test]
    fn valid_terms_keep_their_documented_order() {
        let a = atoms();
        let mut heap = Heap::new(128);
        let list = heap.cons(make_small(1), NIL).unwrap();
        let ordered = [
            make_small(0),
            make_small(1),
            make_atom(a.lookup("a").unwrap()),
            heap.tuple(&[make_small(1)]).unwrap(),
            NIL,
            list,
        ];
        for (i, x) in ordered.iter().enumerate() {
            for (j, y) in ordered.iter().enumerate() {
                assert_eq!(cmp(*x, *y, &a), i.cmp(&j), "sample {i} vs {j}");
                assert_eq!(eq(*x, *y, &a), i == j, "sample {i} vs {j}");
            }
        }
    }

    #[test]
    fn comparison_is_antisymmetric_over_samples() {
        let a = atoms();
        let mut heap = Heap::new(128);
        // SAFETY: the heap owns every block built here.
        let (fun_local, fun_ext) = unsafe {
            (
                local_fun(&mut heap, 3, 1, &[make_small(0)]),
                external_fun(&mut heap, a.lookup("a").unwrap(), a.lookup("b").unwrap(), 1),
            )
        };
        let samples = vec![
            make_small(0),
            make_small(7),
            make_atom(a.lookup("a").unwrap()),
            NIL,
            heap.tuple(&[make_small(1)]).unwrap(),
            heap.cons(make_small(1), NIL).unwrap(),
            fun_local,
            fun_ext,
        ];
        for x in &samples {
            for y in &samples {
                let xy = cmp(*x, *y, &a);
                let yx = cmp(*y, *x, &a);
                assert_eq!(xy, yx.reverse(), "antisymmetry violated for {x:?} {y:?}");
                assert_eq!(eq(*x, *y, &a), xy.is_eq());
            }
        }
    }
}
