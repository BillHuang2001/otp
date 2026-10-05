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
//!   compares by class rank, not numerically — exactly as `erts_cmp` does.
//! * **Tuples** compare elementwise after the arity; **lists** (cons cells)
//!   compare elementwise with the shorter/improper tail deciding a prefix tie;
//!   **bitstrings** compare bit-by-bit (most-significant first) then by bit size;
//!   **internal pids/ports** compare by their raw immediate payload.
//! * **Internal references** are compared by their raw payload words
//!   (most-significant first). The C compares the decoded `num[]` array; the
//!   payload-word comparison is a deterministic proxy that is equivalent as long
//!   as the platform ref layout is stable.
//! * **Big integers, external pid/port/ref, funs, non-flat maps, records and
//!   on-heap bin refs** have a *deterministic fallback*: they are ordered by
//!   class first (which is exact), then by object arity, then by the raw term
//!   word (a stable proxy). This never panics but is not a faithful reproduction
//!   of the C's structural comparison for those specific classes; callers that
//!   need exact ordering for them must extend this module.

use core::cmp::Ordering;

use crate::atom::AtomTable;
use crate::boxed::{boxed_val, header_arity, is_float, HEADER_SUB_BITS};
use crate::eterm::Eterm;
use crate::immediate::{
    atom_val, internal_pid_data, internal_port_data, is_atom, is_small, signed_value,
};
use crate::list::{car, cdr, is_list};
use crate::ops::{class, TermClass};
use crate::tag::BIG_SIGN_BIT;
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
pub fn cmp(a: Eterm, b: Eterm, atoms: &AtomTable) -> Ordering {
    cmp_impl(a, b, atoms, true)
}

/// `=:=` — whether two terms are exactly equal (`cmp(a, b).is_eq()`).
pub fn eq(a: Eterm, b: Eterm, atoms: &AtomTable) -> bool {
    cmp(a, b, atoms).is_eq()
}

/// The arithmetic (`==`) ordering — `erts_cmp(a, b, exact=0, eq_only=0)`.
///
/// Differs from [`cmp`] only where `==` and `=:=` diverge: floats use IEEE
/// comparison (so `-0.0 == +0.0`), and a mixed integer/float pair compares
/// numerically (`1 == 1.0`). Bignum-to-float conversion is a documented
/// approximation (see the module `Known Issues`).
pub fn cmp_arith(a: Eterm, b: Eterm, atoms: &AtomTable) -> Ordering {
    cmp_impl(a, b, atoms, false)
}

/// The shared comparison core, parameterised by `exact`.
fn cmp_impl(a: Eterm, b: Eterm, atoms: &AtomTable, exact: bool) -> Ordering {
    // Identical words are always equal (the C `is_same` fast path).
    if a.raw() == b.raw() {
        return Ordering::Equal;
    }
    if is_atom(a) && is_atom(b) {
        return cmp_atoms(a, b, atoms);
    }
    if is_small(a) && is_small(b) {
        return signed_value(a).cmp(&signed_value(b));
    }
    // SAFETY: `is_float` and `class` only dereference the header word of a live
    // boxed term; every term here was produced by this crate.
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
        // Documented deterministic fallback (see the module `Known Issues`).
        _ => fallback_cmp(a, b),
    }
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

/// The `f64` value stored in a boxed float term.
///
/// # Safety
///
/// `x` must be a live boxed float (`is_float(x)`).
pub(crate) unsafe fn float_value(x: Eterm) -> f64 {
    f64::from_bits(unsafe { float_bits(x) })
}

/// The raw `f64` bits stored in a boxed float term.
///
/// # Safety
///
/// `x` must be a live boxed float (`is_float(x)`).
pub(crate) unsafe fn float_bits(x: Eterm) -> u64 {
    // The 64-bit flonum is a 1-word thing: value at `boxed_val(x)[1]`.
    unsafe { (*boxed_val(x).add(1)).raw() as u64 }
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
                // SAFETY: both are live numeric terms.
                unsafe { number_as_f64(a).partial_cmp(&number_as_f64(b)) }
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

/// Best-effort `f64` value of a numeric term (used only by arithmetic mixed
/// comparisons). Bignum conversion may lose precision.
///
/// # Safety
///
/// `x` must be a live numeric term.
unsafe fn number_as_f64(x: Eterm) -> f64 {
    match unsafe { class(x) } {
        TermClass::Small => signed_value(x) as f64,
        TermClass::Float => unsafe { float_value(x) },
        TermClass::Big => {
            // SAFETY: `x` is a boxed bignum.
            let p = boxed_val(x);
            let hdr = unsafe { *p };
            let arity = header_arity(hdr);
            let mut value = 0.0f64;
            let mut i = arity;
            while i > 0 {
                i -= 1;
                // SAFETY: limb `i` is within the bignum's payload.
                value = value * 18_446_744_073_709_551_616.0 + unsafe { (*p.add(1 + i)).raw() } as f64;
            }
            if big_is_negative(x) {
                -value
            } else {
                value
            }
        }
        _ => f64::NAN,
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
    let (ab, ao, asz) = unsafe { bitstring_parts(a) };
    let (bb, bo, bsz) = unsafe { bitstring_parts(b) };
    let n = asz.min(bsz);
    let mut i = 0;
    while i < n {
        // SAFETY: bit `ao + i` / `bo + i` is within the respective bitstring.
        let abit = unsafe { bit_at(ab, ao + i) };
        let bbit = unsafe { bit_at(bb, bo + i) };
        if abit != bbit {
            return abit.cmp(&bbit);
        }
        i += 1;
    }
    asz.cmp(&bsz)
}

/// Read bit `bit` (0 = most significant of the first byte) of a bitstring base.
///
/// # Safety
///
/// `base` must point at the bit data of a bitstring covering bit `bit`.
pub(crate) unsafe fn bit_at(base: *const u8, bit: usize) -> u8 {
    // SAFETY: the caller guarantees the byte is in range.
    unsafe { (*base.add(bit >> 3) >> (7 - (bit & 7))) & 1 }
}

/// Decompose a bitstring into `(base byte pointer, start bit offset, bit size)`.
///
/// # Safety
///
/// `x` must be a live bitstring (heap bits or sub bits) term, and — for sub bits
/// — the base pointer must still be valid (i.e. it must not be a volatile
/// writable binary).
pub(crate) unsafe fn bitstring_parts(x: Eterm) -> (*const u8, usize, usize) {
    // SAFETY: `x` is a live boxed bitstring.
    let p = boxed_val(x);
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // ErlSubBits: [thing_word, base_flags, start, end, orig].
        let base_flags = unsafe { *p.add(1) }.raw();
        let start = unsafe { *p.add(2) }.raw();
        let end = unsafe { *p.add(3) }.raw();
        ((base_flags & !3usize) as *const u8, start, end - start)
    } else {
        // ErlHeapBits: [thing_word, size, data...].
        let size = unsafe { *p.add(1) }.raw();
        (unsafe { p.add(2) } as *const u8, 0, size)
    }
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
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data, make_small, NIL};
    use crate::tag::{HAMT_SUBTAG_HEAD_FLATMAP, HEAP_BITS_SUBTAG, REF_SUBTAG};

    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "b", "c", "z"]);
        t
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
    fn comparison_is_antisymmetric_over_samples() {
        let a = atoms();
        let mut heap = Heap::new(128);
        let samples = vec![
            make_small(0),
            make_small(7),
            make_atom(a.lookup("a").unwrap()),
            NIL,
            heap.tuple(&[make_small(1)]).unwrap(),
            heap.cons(make_small(1), NIL).unwrap(),
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
