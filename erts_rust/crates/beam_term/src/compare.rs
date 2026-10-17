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
//! `small < big < float < atom < external_ref < ref < fun < external_port < port <
//! external_pid < pid < tuple < record < map < nil < list < bitstring`, with the C's
//! `bin_ref` class last. [`class_rank`] derives those positions from the `*_DEF`
//! virtual-tag constants in `erts/emulator/beam/erl_term.h:1441-1457`: the rank is
//! `0x10 - _DEF` over the 17 real classes (`SMALL_DEF` `0x10` ⇒ 0 … `BITSTRING_DEF`
//! `0x00` ⇒ 16), i.e. term order ascends as `_DEF` descends. The C's `BIN_REF_DEF`
//! (`0x11`, `:1458`, documented there as "not a real term") is outside that ladder
//! and is placed after every real class (rank 17), and this crate's
//! [`TermClass::NonValue`] (a bare header word — see below) after that. These ranks
//! decide any pair of *different* classes; two numeric classes are compared by
//! [`numbers`] instead (by rank in exact mode, numerically in arithmetic mode).
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
//!   *arithmetic* mode a mixed integer/float pair is compared by ERTS's **exact**
//!   algorithm (`SMALL_FLOAT`/`FLOAT_SMALL`/`BIG_FLOAT`/`FLOAT_BIG`, ported in
//!   [`numbers`]) — never by lossily converting both sides to `f64`, so
//!   `9007199254740993 == 9007199254740992.0` is `false` and
//!   `9007199254740993 > 9007199254740992.0` is `true`, as in the real runtime.
//!   ([`crate::number::number_as_f64`] is *not* used on this path.)
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
//!   Those structural arms are reached only when *both* operands are external (or
//!   both records): a **mixed internal/external** pair of the same category is
//!   decided by [`class_rank`] instead, whereas the C's `pid_common`/`ref_common`
//!   arms (`utils.c:1777`/`:2262`, reached from the external cases at
//!   `:2212`/`:2235`/`:2288`) compare `serial`, `number` and the node first. The two
//!   rules agree when the foreign sysname sorts before the local one and disagree
//!   otherwise — the one known ordering gap for these classes; it is pinned by
//!   `tests/compare_class_order.rs`.//! * **On-heap bin refs** have a *deterministic fallback*: they are ordered by
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
use crate::fun::{fun_desc_words, fun_kind_of_header, fun_num_free, FunKind};
use crate::immediate::{
    atom_val, internal_pid_data, internal_port_data, is_atom, is_small, signed_value,
};
use crate::list::{car, cdr, is_list};
use crate::ops::{class, TermClass};
use crate::record::{
    record_definition, record_definition_is_exported, record_definition_keys,
    record_definition_module, record_definition_name, record_field_count, record_values,
};
use crate::tag::is_term_word;
use crate::thing::{external_pid_parts, external_port_parts, external_ref_parts};
use crate::tuple::{tuple_element, tuple_size};

/// The numeric arm of the comparison — `erts_cmp_compound`'s `mixed_types` block
/// and the `big`/`float` helpers it dispatches to. Kept in its own file so the
/// exact integer ↔ float algorithm stays under its own roof (and file-size budget).
mod numbers;
/// The map arm of the comparison — flat maps compare straight out of their storage
/// (no allocation), everything else through the collecting fallback. See the module
/// docs for the flat-map order precondition.
mod maps;

use maps::cmp_maps;
use numbers::cmp_numbers;
/// Ascending term-order rank of a [`TermClass`].
///
/// `rank = 0x10 - _DEF` over the 17 real classes of the C's `*_DEF` ladder
/// (`erts/emulator/beam/erl_term.h:1441-1457`), with `BIN_REF_DEF` (`0x11`, "not a
/// real term") placed after every real class and [`TermClass::NonValue`] after that;
/// a smaller rank sorts first. See the module docs.
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
/// Two flat maps whose keys-tuples are already in [`cmp_flatmap_keys`] order are
/// compared straight out of their storage — no pair list, no sort, no allocation
/// (see [`maps`]); everything else uses the collecting fallback. Both paths
/// compare the same two sequences, so they cannot disagree.
///
/// Both operands are, by the caller invariant, well-formed maps. A malformed
/// map (only reachable if the term was forged outside this crate) falls back to
/// the deterministic [`fallback_cmp`] rather than panicking.
///
/// (The body lives in [`maps::cmp_maps`], which this re-export routes to.)
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

/// Compare two tuples: by arity first, then elementwise.///
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
///
/// `pub(crate)`, together with [`record_parts`] and [`exported_rank`], so
/// [`crate::hashing`]'s structural `term_hash` iterates exactly these words in
/// exactly this order — the hash and the comparison cannot drift apart.
pub(crate) struct RecordParts<'a> {
    pub(crate) field_count: usize,
    pub(crate) module: Eterm,
    pub(crate) name: Eterm,
    pub(crate) is_exported: Eterm,
    pub(crate) keys: &'a [Eterm],
    pub(crate) values: &'a [Eterm],
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
pub(crate) unsafe fn record_parts<'a>(t: Eterm) -> Option<RecordParts<'a>> {
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
///
/// `pub(crate)` so [`crate::hashing`]'s structural record hash mixes the *same*
/// comparable value [`cmp_records`] compares.
pub(crate) fn exported_rank(is_exported: Eterm) -> u32 {
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
/// * `External` ⇒ `(module, function, arity)` — the arity being descriptor word
///   3, the `Export`'s **true** `info.mfa.arity` (`crate::fun::fun_true_arity`),
///   not the header's truncated 8-bit field.
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
            // Descriptor word 3 is the `Export`'s `info.mfa.arity` — the TRUE arity,
            // which the C compares here (`utils.c:2203-2206`). The header's 8-bit
            // field would be wrong for an arity above 255.
            // SAFETY: as above — descriptor word 3.
            let (ara, arb) = unsafe { ((*pa.add(3)).raw(), (*pb.add(3)).raw()) };
            ara.cmp(&arb)
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
