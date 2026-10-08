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

//! Object word size (`size_object` / `erts_size_object`).
//!
//! Ground truth: `size_object_x(obj, NULL)` in `erts/emulator/beam/copy.c`,
//! exported as `#define size_object(Term) size_object_x(Term,NULL)` in
//! `erts/emulator/beam/global.h`. It returns the **flat** number of heap words
//! the term graph occupies — the count `copy_struct` uses to size a destination
//! heap, and the amount `beam_gc`/`beam_message` must reserve.
//!
//! The convention (confirmed against `size_object_x`):
//!
//! * an **immediate** (small int, atom, `nil`, internal pid/port, catch) → `0`
//!   words;
//! * a **cons cell** → `2` words (`[CAR, CDR]`, header-less), **plus** the sizes
//!   of its car and cdr;
//! * a **boxed object** → `arity + 1` words (header + payload), **plus** the
//!   sizes of any nested terms it contains.
//!
//! so `size_object(42) == 0`, `size_object([1]) == 2`,
//! `size_object({1,2,3}) == 4`, and `size_object([1,2,3]) == 6`.
//!
//! # Bounded, cycle-safe sizing
//!
//! `size_object_x` walks the graph **unconditionally**: it recurses per occurrence
//! and has no notion of a budget, so a *cyclic* graph — a compound object reachable
//! from itself, which only forged memory (a NIF handing back a graph pointing into
//! an ancestor) can produce — sends the C into infinite recursion. This crate's walk
//! is an explicit-stack DFS that keeps the compound objects on the **current path**,
//! so it stops at the back-edge instead of looping: [`size_object`] returns the words
//! counted up to it (a defined answer, documented on the function), and
//! [`size_object_bounded`] reports the typed
//! [`TermError::TermGraphCyclic`]/[`TermError::TermGraphTooLarge`] distinction.
//!
//! Both entry points share **one** walk (`sizing_walk`), which is the single place
//! that knows how many words each object contributes and which of its slots are
//! children (`describe_object`).
//!
//! # Internal layouts
//!
//! Like the C, every navigable interior is walked:
//!
//! * a **hash-map (HAMT) node** — head-array, head-bitmap and inner node-bitmap
//!   alike — contributes `1 + popcount(MAP_HEADER_VAL) + header_arity` words and
//!   every non-immediate slot is traversed, so the whole trie (leaves, sub-nodes
//!   and collision tuples) is counted (`copy.c:192-214`);
//! * an **`ErlSubBits`** (`SUB_BITS_SUBTAG`) whose `orig` is a `BinRef` is a full
//!   refc binary and contributes the whole on-heap `[BinRef(3)][ErlSubBits(5)]`
//!   pair ([`ERL_REFC_BITS_SIZE`], `8` words); a match context over an on-heap
//!   bitstring instead contributes the size of the *copied* window
//!   (`copy.c:219-242`).
//!
//! # Known Issues
//!
//! * **Native records** (`RECORD_SUBTAG`) are treated like tuples (their
//!   field-table layout is not yet modelled).

use std::collections::HashSet;

use crate::bitstring::{heap_bits_size_words, sub_bits_parts};
use crate::boxed::{
    boxed_val, header_arity, is_boxed_term, is_flatmap_header, map_header_val, thing_arityval,
    ERL_SUB_BITS_SIZE, HEADER_BIN_REF,
};
use crate::error::TermError;
use crate::eterm::Eterm;
use crate::fun::{fun_desc_words, fun_kind_of_header, fun_num_free, fun_words};
use crate::immediate::is_immediate;
use crate::list::{is_list, list_val};
use crate::refc::ERL_REFC_BITS_SIZE;
use crate::tag::{
    is_term_word, ARITYVAL_SUBTAG, FUN_SUBTAG, MAP_SUBTAG, RECORD_SUBTAG, SUB_BITS_SUBTAG,
    TAG_HEADER_MASK,
};

/// Whether a word can contribute heap words, so the walk must enter it: a term word
/// that is not an immediate.
///
/// A word that is **not a term** (a bare header word — the ABI's `THE_NON_VALUE` and
/// friends, see [`crate::tag::is_term_word`]) and an immediate both contribute `0` to
/// every count, so the walk never stacks them.
#[inline]
fn needs_visit(t: Eterm) -> bool {
    is_term_word(t) && !is_immediate(t)
}

/// The navigable child slots of one compound object.
enum Children {
    /// A cons cell's two words: `[CAR, CDR]` (`copy.c:107-111`).
    Cons { head: Eterm, tail: Eterm },
    /// `count` consecutive term slots starting at `base`, a pointer *into* the
    /// object's payload.
    Slots { base: *mut Eterm, count: usize },
    /// The object exposes no navigable child: a float, bignum, ref, pid, port,
    /// external thing, heap bitstring, or an on-heap refc-binary pair (whose off-heap
    /// payload the C never descends into).
    None,
}

/// One compound object: the words it contributes, the address that identifies it on a
/// DFS path, and its navigable child slots.
struct Object {
    /// The object's address — the identity a DFS back-edge must match.
    addr: usize,
    /// The flat words the object itself contributes.
    words: usize,
    /// Its navigable child slots.
    children: Children,
}

/// One entry of the explicit DFS stack both walks use.
enum Step {
    /// Enter a word. It may or may not turn out to be a compound object.
    Enter(Eterm),
    /// Leave the compound object at this address: it is no longer on the DFS path.
    Exit(usize),
}

/// Describes the compound object `t` — the **single** definition of an object's
/// interior: the words it contributes, the address identifying it, and its
/// navigable children — or `None` when `t` contributes no heap words and exposes no
/// child.
///
/// `None` covers exactly the words `size_object_x` never descends into:
///
/// * an **immediate** (small int, atom, `nil`, internal pid/port, catch) — 0 words;
/// * a word that is **not a term at all** (a bare header word: the ABI's
///   `THE_NON_VALUE` and friends) — it is not a term graph, so it occupies no heap
///   word; the C dereferences it (`*boxed_val(0)`), this is the documented hardening;
/// * the shared **zero-arity tuple literal** (`tuple0()`), which the C also skips
///   (its `arity == 0` guard).
///
/// # Safety
///
/// The caller must pass a live word: if it is a boxed term its header word must be a
/// live term word, and every child slot it exposes must be readable. That is the
/// same caller invariant [`crate::compare::cmp`] and every other materialization
/// entry point in this crate documents; forged memory (a `HEADER`-tagged word is
/// rejected, but a `BOXXED`/`LIST`-tagged word pointing outside any heap is not) is
/// outside what a crate without a heap registry can detect.
unsafe fn describe_object(t: Eterm) -> Option<Object> {
    if !needs_visit(t) {
        return None;
    }
    if is_list(t) {
        // A cons cell is a header-less 2-word object.
        let p = list_val(t);
        // SAFETY: `t` is a live, non-`NIL` cons cell, so both words are readable.
        let (head, tail) = unsafe { (*p, *p.add(1)) };
        return Some(Object {
            addr: p as usize,
            words: 2,
            children: Children::Cons { head, tail },
        });
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a live boxed term, so `p` addresses its header word.
    let header = unsafe { *p };
    match header.raw() & TAG_HEADER_MASK {
        ARITYVAL_SUBTAG | RECORD_SUBTAG => {
            let arity = header_arity(header);
            // The zero-arity tuple is the shared global literal: 0 words.
            if arity == 0 {
                return None;
            }
            Some(Object {
                addr: p as usize,
                words: arity + 1,
                // SAFETY: element slot `0` is the first of the `arity` payload words.
                children: Children::Slots {
                    base: unsafe { p.add(1) },
                    count: arity,
                },
            })
        }
        FUN_SUBTAG => {
            // A fun block is `1 (header) + desc_words(kind) + num_free`
            // (see `crate::fun`): the M1 inlined descriptor replaces the C
            // `entry` pointer, so the env does **not** start at the C
            // `ERL_FUN_SIZE`. `fun_words` is that same expression.
            let kind = fun_kind_of_header(header);
            let num_free = fun_num_free(header);
            Some(Object {
                addr: p as usize,
                words: fun_words(kind, num_free),
                // SAFETY: the environment starts at `1 + desc_words(kind)` and holds
                // `num_free` slots within the block `fun_words` sized.
                children: Children::Slots {
                    base: unsafe { p.add(1 + fun_desc_words(kind)) },
                    count: num_free,
                },
            })
        }
        MAP_SUBTAG => {
            if is_flatmap_header(header) {
                // flatmap_t = [thing_word, size, keys(tuple), V0..V(size-1)].
                // SAFETY: word `1` is the flat map's `size` field.
                let size = unsafe { *p.add(1) }.raw();
                Some(Object {
                    addr: p as usize,
                    words: size + 3,
                    // SAFETY: word `2` is the keys tuple, followed by the `size`
                    // values.
                    children: Children::Slots {
                        base: unsafe { p.add(2) },
                        count: size + 1,
                    },
                })
            } else {
                // Every other map header is a HAMT node: a head-array, a
                // head-bitmap or an inner node-bitmap. `header_arity(header)`
                // is `1` for the two head kinds (their extra `size` word) and
                // `0` for an inner node, so the slots start at
                // `1 + header_arity`. Mirrors `size_object_x`
                // (`copy.c:192-214`): `sum += 1 + popcount(MAP_HEADER_VAL) +
                // header_arity`, then traverses every slot so the whole trie
                // (leaves, sub-nodes and collision tuples) is reached.
                let arity = header_arity(header);
                let nslots = map_header_val(header).count_ones() as usize;
                Some(Object {
                    addr: p as usize,
                    words: 1 + nslots + arity,
                    // SAFETY: the `nslots` slots follow the `arity` offset words.
                    children: Children::Slots {
                        base: unsafe { p.add(1 + arity) },
                        count: nslots,
                    },
                })
            }
        }
        SUB_BITS_SUBTAG => {
            // `[HEADER_SUB_BITS, base_flags, start, end, orig]` (`copy.c:219-242`).
            // SAFETY: `t` is a live `ErlSubBits` block.
            let (_base_flags, start, end, orig) = unsafe { sub_bits_parts(t) };
            let words = if is_boxed_term(orig) {
                // SAFETY: `orig` is a boxed term; the C reads
                // `*boxed_val(sb->orig)` unconditionally (`copy.c:222`).
                let underlying = unsafe { *boxed_val(orig) };
                if underlying == HEADER_BIN_REF {
                    // A full refc binary: the on-heap representation is the
                    // `[BinRef(3)][ErlSubBits(5)]` pair whose root is this
                    // `ErlSubBits`; the C counts the whole pair in one go
                    // (the `BinRef`'s own `val`/`next` are off-heap scalar
                    // pointers contributing no heap words).
                    ERL_REFC_BITS_SIZE
                } else {
                    // A match context over an on-heap bitstring: the copy
                    // materialises a fresh heap-bits block for the window
                    // (which is why the underlying thing is not traversed).
                    heap_bits_size_words(end.raw().saturating_sub(start.raw()))
                }
            } else {
                // Not a term the C accepts: `boxed_val(sb->orig)` would
                // dereference a bogus pointer there. A standalone/malformed
                // `ErlSubBits` reports its own thing size instead.
                ERL_SUB_BITS_SIZE
            };
            Some(Object {
                addr: p as usize,
                words,
                children: Children::None,
            })
        }
        _ => Some(Object {
            // Float, bignum, ref, bin ref, heap bits, external pid/port/ref:
            // header + payload. None of these has navigable subterms.
            addr: p as usize,
            words: thing_arityval(header) + 1,
            children: Children::None,
        }),
    }
}

/// Pushes every navigable child of `children` onto a DFS `stack`.
fn push_children(children: &Children, stack: &mut Vec<Step>) {
    match children {
        Children::None => {}
        Children::Cons { head, tail } => {
            if needs_visit(*tail) {
                stack.push(Step::Enter(*tail));
            }
            if needs_visit(*head) {
                stack.push(Step::Enter(*head));
            }
        }
        Children::Slots { base, count } => {
            for i in 0..*count {
                // SAFETY: `base` addresses `count` live term slots of the object the
                // `Children::Slots` value was derived from.
                let child = unsafe { *base.add(i) };
                if needs_visit(child) {
                    stack.push(Step::Enter(child));
                }
            }
        }
    }
}

/// Where [`sizing_walk`] stopped, and the words it had counted by then.
enum Sizing {
    /// The whole graph was walked: the count is the term's flat size. No limit was
    /// exceeded and no cycle was found.
    Complete(usize),
    /// The running total passed the walk's `limit` before the graph was exhausted.
    OverLimit(usize),
    /// A compound object was reached while still on the DFS path: the graph is
    /// **cyclic**, so it has no flat size. `usize` is the count accumulated up to the
    /// back-edge, which is what makes [`size_object`] total on such a graph.
    Cyclic(usize),
}

/// The cycle-safe, `limit`-bounded sizing walk shared by [`size_object`] and
/// [`size_object_bounded`].
///
/// It is `size_object_x`'s walk (`copy.c:88-263`) turned into an explicit-stack DFS
/// with a **path set**: `on_path` holds the addresses of the compound objects on the
/// current DFS path, so reaching one of them again is a back-edge — a cycle — and the
/// walk stops there instead of looping. Sharing an object between two *sibling*
/// positions is not a cycle (the first subtree leaves the path before the second
/// enters it), so a valid graph that shares a literal/boxed subterm is still sized
/// once **per occurrence**, exactly as `size_object_x`/`copy_struct` count it. A
/// global "already seen" set would under-count such a graph, which is why the set is
/// path-based.
///
/// The `limit` bounds the work: [`Sizing::OverLimit`] is returned as soon as the
/// running total passes it, so a huge (or cyclic) graph cannot make the walk run
/// away. The total only ever counts objects actually entered, so on an **acyclic**
/// graph `OverLimit` means the true flat size really does exceed `limit`.
///
/// The walk is iterative — a deep list cannot overflow the native stack — and uses
/// `O(visited compound objects)` stack/`on_path` memory.
fn sizing_walk(root: Eterm, limit: usize) -> Sizing {
    let mut total = 0usize;
    let mut on_path: HashSet<usize> = HashSet::new();
    let mut stack: Vec<Step> = vec![Step::Enter(root)];

    while let Some(step) = stack.pop() {
        match step {
            Step::Exit(addr) => {
                on_path.remove(&addr);
            }
            Step::Enter(t) => {
                // SAFETY: the walk is entered with a live word and every child it
                // exposes is a live word of the same graph.
                let Some(object) = (unsafe { describe_object(t) }) else {
                    // An immediate / non-term / shared empty tuple literal: 0 words.
                    continue;
                };
                let has_children = !matches!(object.children, Children::None);
                if has_children && !on_path.insert(object.addr) {
                    // A back-edge: the object is one of its own ancestors.
                    return Sizing::Cyclic(total);
                }
                total = total.saturating_add(object.words);
                if total > limit {
                    return Sizing::OverLimit(total);
                }
                if has_children {
                    stack.push(Step::Exit(object.addr));
                    push_children(&object.children, &mut stack);
                }
            }
        }
    }
    Sizing::Complete(total)
}

/// Whether the term graph rooted at `root` is **acyclic**.
///
/// The same DFS and the same `describe_object` child enumeration as [`sizing_walk`],
/// but it accounts no words and adds a `done` set, making it the classic three-colour
/// cycle check: a revisit of an object on the path is a back-edge (a cycle), and an
/// object already finished (`done`) needs no second look. That keeps it in
/// `O(V + E)` work over the *distinct* objects with `O(V)` memory (where a path-based
/// check would re-walk a shared subterm once per path).
///
/// It is only ever called on the **error** path of [`size_object_bounded`] — after the
/// bounded walk has already shown the graph does not fit — so its sets are never paid
/// on the happy path. It is what lets [`TermError::TermGraphCyclic`] win over
/// [`TermError::TermGraphTooLarge`] even when the limit is hit before the cycle-closing
/// edge is reached.
fn is_acyclic(root: Eterm) -> bool {
    let mut on_path: HashSet<usize> = HashSet::new();
    let mut done: HashSet<usize> = HashSet::new();
    let mut stack: Vec<Step> = vec![Step::Enter(root)];

    while let Some(step) = stack.pop() {
        match step {
            Step::Exit(addr) => {
                on_path.remove(&addr);
                done.insert(addr);
            }
            Step::Enter(t) => {
                // SAFETY: as in `sizing_walk` — a live word and live children.
                let Some(object) = (unsafe { describe_object(t) }) else {
                    continue;
                };
                let has_children = !matches!(object.children, Children::None);
                if has_children {
                    if on_path.contains(&object.addr) {
                        // A back-edge: the object is one of its own ancestors.
                        return false;
                    }
                    if done.contains(&object.addr) {
                        // Fully explored already, with no cycle below it: skip.
                        continue;
                    }
                    on_path.insert(object.addr);
                    stack.push(Step::Exit(object.addr));
                }
                push_children(&object.children, &mut stack);
            }
        }
    }
    true
}

/// The flat number of heap words the term graph `a` occupies.
///
/// Reproduces `size_object(a)` == `size_object_x(a, NULL)`. See the module docs
/// for the convention and the documented approximations.
///
/// A word that is **not a term** (a bare header word — the ABI's `THE_NON_VALUE`
/// and friends; see [`crate::tag::is_term_word`]) contributes `0` words: it is not
/// a term graph at all, so no heap word belongs to it. The C dereferences such a
/// word (`*boxed_val(0)`), so this is a documented, panic-free hardening — the
/// size of every valid term is unchanged.
///
/// A **cyclic** graph is the one case where no flat size exists. Where
/// `size_object_x` recurses forever, this walk stops at the back-edge and returns the
/// words counted up to it — the part of the walk *before* the edge that closed the
/// cycle (the cycle-closing object is not counted a second time). That number is
/// deliberately **not** a meaningful size: it exists so this function is total and
/// never hangs. Use [`size_object_bounded`] to learn that a graph is cyclic.
pub fn size_object(a: Eterm) -> usize {
    // The unbounded walk: only `Complete` and `Cyclic` can come back (the saturated
    // total of a real graph is `<= usize::MAX == limit`).
    match sizing_walk(a, usize::MAX) {
        Sizing::Complete(words) | Sizing::OverLimit(words) | Sizing::Cyclic(words) => words,
    }
}

/// [`size_object`] with an explicit word budget, and with the reason a graph cannot
/// be sized reported as a typed error instead of a count.
///
/// The precise invariant — for the whole graph reachable from `a`:
///
/// * **acyclic** and its flat word count `n` satisfies `n <= limit` → `Ok(n)`,
///   identical to [`size_object`]`(a)`;
/// * **acyclic** and `n > limit` → `Err(`[`TermError::TermGraphTooLarge`]`)`;
/// * **cyclic** (a compound object reachable from itself) →
///   `Err(`[`TermError::TermGraphCyclic`]`)`, which takes priority over
///   `TermGraphTooLarge` even when the limit is hit before the cycle-closing edge is
///   reached: the bounded walk stops at the budget, then a second, acyclicity-only
///   pass (never paid on the happy path, see `is_acyclic`) tells the two apart;
/// * a word that is **not a term** ([`crate::tag::is_term_word`] false — the ABI's
///   `THE_NON_VALUE` and other bare header words) contributes `0` and is never
///   dereferenced, as does a non-term slot inside an otherwise valid container.
///
/// The walk is iterative, so a deep list cannot overflow the native stack, and it
/// terminates in `O(visited compound objects)` work with `O(visited)` memory — a
/// forged cyclic graph is a typed error, never a hang. "Visited" counts
/// *occurrences*, not distinct objects: like `size_object_x`, a subterm a valid graph
/// shares between two sibling positions is sized once per occurrence.
pub fn size_object_bounded(a: Eterm, limit: usize) -> Result<usize, TermError> {
    match sizing_walk(a, limit) {
        Sizing::Complete(words) => Ok(words),
        // The back-edge was reached before the budget: unambiguously cyclic.
        Sizing::Cyclic(_) => Err(TermError::TermGraphCyclic),
        Sizing::OverLimit(_) => {
            if is_acyclic(a) {
                Err(TermError::TermGraphTooLarge)
            } else {
                Err(TermError::TermGraphCyclic)
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstring::{write_heap_bits, write_sub_bits};
    use crate::boxed::{
        arityval, is_arity_value, is_map_header, make_arityval, make_boxed, make_header,
        map_header_type, HEADER_FLONUM,
    };
    use crate::heap::Heap;
    use crate::fun::{write_fun_external, write_fun_imported, write_fun_local, FunKind, FunPid};
    use crate::immediate::{
        make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data,
        make_small, NIL,
    };
    use crate::map::{make_map_header, map_words_capacity, write_map};
    use crate::refc::write_refc_binary;
    use crate::tag::{
        HEAP_BITS_SUBTAG, MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
        MAP_HEADER_TAG_HAMT_NODE_BITMAP,
    };
    use crate::tuple::tuple0;
    use crate::AtomTable;
    use crate::Atom;

    #[test]
    fn immediates_have_zero_size() {
        let mut table = crate::AtomTable::new();
        let atom = make_atom(table.intern("some_atom"));
        for t in [
            make_small(0),
            make_small(1234),
            atom,
            NIL,
            make_internal_pid(make_pid_data(0, 5)),
            make_internal_port(make_port_data(5)),
        ] {
            assert_eq!(size_object(t), 0, "immediate {t:?} should be 0 words");
        }
    }

    #[test]
    fn cons_cells_are_two_words_each() {
        let mut heap = Heap::new(16);
        let one = heap.cons(make_small(1), NIL).unwrap();
        assert_eq!(size_object(one), 2);
        let tail3 = heap.cons(make_small(3), NIL).unwrap();
        let tail23 = heap.cons(make_small(2), tail3).unwrap();
        let three = heap.cons(make_small(1), tail23).unwrap();
        assert_eq!(size_object(three), 6);
    }

    #[test]
    fn tuples_are_header_plus_payload() {
        let mut heap = Heap::new(16);
        let t3 = heap.tuple(&[make_small(1), make_small(2), make_small(3)]).unwrap();
        assert_eq!(size_object(t3), 4);
        // Nested: outer arity 2 (=> 3 words) + inner arity 2 (=> 3 words).
        let inner = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let outer = heap.tuple(&[inner, make_small(9)]).unwrap();
        assert_eq!(size_object(outer), 6);
    }

    #[test]
    fn zero_arity_tuple_literal_is_zero_words() {
        assert_eq!(size_object(tuple0()), 0);
    }

    #[test]
    fn funs_are_header_plus_descriptor_plus_env() {
        // A local fun is `1 (header) + 2 (uniq, index) + num_free`.
        let mut heap = Heap::new(64);
        let free = [make_small(1), make_small(2)];
        // SAFETY: `fun_words` sizes the block exactly; the heap owns it.
        let local = unsafe {
            let hp = heap.alloc_words(fun_words(FunKind::Local, free.len())).unwrap();
            write_fun_local(hp, 2, 0x1234, 5, &free).unwrap()
        };
        assert_eq!(size_object(local), 3 + 2);

        // An exported (`EXPORT_EXT`) fun has no environment: `1 + 2`.
        // SAFETY: `fun_words` sizes the block exactly; the heap owns it.
        let external = unsafe {
            let hp = heap.alloc_words(fun_words(FunKind::External, 0)).unwrap();
            write_fun_external(hp, 2, Atom::from_raw(1), Atom::from_raw(2)).unwrap()
        };
        assert_eq!(size_object(external), 3);

        // A wire fun is `1 (header) + 10 (descriptor) + num_free`.
        for n in [0usize, 1, 3] {
            let env = vec![make_small(9); n];
            // SAFETY: `fun_words` sizes the block exactly; the heap owns it.
            let imported = unsafe {
                let hp = heap.alloc_words(fun_words(FunKind::Imported, n)).unwrap();
                write_fun_imported(
                    hp,
                    0,
                    Atom::from_raw(1),
                    &[0u8; 16],
                    1,
                    2,
                    3,
                    FunPid {
                        node: Atom::from_raw(2),
                        number: 4,
                        serial: 5,
                        creation: 6,
                    },
                    &env,
                )
                .unwrap()
            };
            assert_eq!(size_object(imported), 11 + n, "{n} free variables");
        }
    }

    #[test]
    fn fun_environment_terms_are_counted() {
        let mut heap = Heap::new(64);
        let inner = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let free = [inner, make_small(0)];
        // SAFETY: `fun_words` sizes the block exactly; the heap owns it.
        let local = unsafe {
            let hp = heap.alloc_words(fun_words(FunKind::Local, free.len())).unwrap();
            write_fun_local(hp, 1, 1, 0, &free).unwrap()
        };
        // 5 words for the fun plus the 3-word tuple it closes over.
        assert_eq!(size_object(local), 5 + 3);
    }    #[test]
    fn boxed_leaf_objects_are_header_plus_payload() {
        // A 64-bit flonum is a 1-word thing => 2 words total.
        let float_words: [Eterm; 2] = [HEADER_FLONUM, Eterm::from_raw(0)];
        let float = make_boxed(float_words.as_ptr());
        assert_eq!(size_object(float), 2);

        // A 16-bit heap binary: [thing_word, size, 1 data word] => 3 words.
        let mut heap = Heap::new(4);
        // SAFETY: the heap owns these words.
        let p = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *p = make_header(2, HEAP_BITS_SUBTAG);
            *p.add(1) = Eterm::from_raw(16);
            *p.add(2) = Eterm::from_raw(0x1234);
        }
        let bin = make_boxed(p);
        assert_eq!(size_object(bin), 3);
    }

    #[test]
    fn flatmap_counts_header_size_keys_and_values() {
        // SAFETY: the heap owns every word built here.
        let mut heap = Heap::new(16);
        let keys = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let p = unsafe { heap.alloc_words(5).unwrap() };
        unsafe {
            *p = make_header(0, crate::tag::HAMT_SUBTAG_HEAD_FLATMAP);
            *p.add(1) = Eterm::from_raw(2); // flatmap size
            *p.add(2) = keys;
            *p.add(3) = make_small(7);
            *p.add(4) = make_small(8);
        }
        let map = make_boxed(p);
        // 2 + 3 = 5 words for the flatmap itself, plus the 3-word keys tuple.
        assert_eq!(size_object(map), 5 + 3);
    }

    #[test]
    fn size_is_additive_over_subterms() {
        let mut heap = Heap::new(32);
        let leaf = make_small(1);
        assert_eq!(size_object(leaf), 0);
        let t1 = heap.tuple(&[leaf]).unwrap();
        let l1 = heap.cons(t1, NIL).unwrap();
        // cons (2) + tuple {1} (2) + immediate (0)
        assert_eq!(size_object(l1), 4);
    }

    // -----------------------------------------------------------------------
    // Regression tests: a HAMT (hash map) trie and a refc binary must be counted
    // in full. Both used to be under-counted (only the head node resp. only the
    // on-heap `ErlSubBits` thing size).
    // -----------------------------------------------------------------------

    /// A map laid out into an owned buffer that outlives the returned term.
    struct BuiltMap {
        _buf: Vec<Eterm>,
        map: Eterm,
    }

    /// Lays `pairs` out with [`write_map`] into a buffer of exactly
    /// [`map_words_capacity`]`(pairs.len())` words.
    fn build_map(pairs: &[(Eterm, Eterm)]) -> BuiltMap {
        let atoms = AtomTable::new();
        let cap = map_words_capacity(pairs.len());
        let mut buf = vec![Eterm::from_raw(0xDEAD_BEEF); cap];
        // SAFETY: `buf` owns `cap` words and is kept alive by the returned value.
        let map = unsafe { write_map(buf.as_mut_ptr(), pairs, &atoms) }.unwrap();
        BuiltMap { _buf: buf, map }
    }

    /// `n` distinct integer keys; the values are immediates and add no words.
    fn int_pairs(n: i64) -> Vec<(Eterm, Eterm)> {
        (1..=n)
            .map(|k| (make_small(k), make_small(k * 7)))
            .collect()
    }

    /// A two-word `[Key, Value]` leaf cell allocated from `heap`.
    fn leaf(heap: &mut Heap, key: i64) -> Eterm {
        heap.cons(make_small(key), make_small(key * 10)).unwrap()
    }

    /// What an independent walk of a HAMT trie found.
    #[derive(Default)]
    struct HamtWalk {
        /// One per key.
        leaves: usize,
        /// The words of every node (header, `size` word and slots) and of every
        /// collision-node tuple.
        node_words: usize,
        inner_nodes: usize,
        collision_nodes: usize,
        head_array: bool,
        head_bitmap: bool,
    }

    impl HamtWalk {
        /// The whole map's words: its node words plus two per leaf cons cell.
        fn total(&self) -> usize {
            self.node_words + 2 * self.leaves
        }
    }

    /// Counts a HAMT trie node by node, deriving each node's size **by hand** from
    /// the layout table in [`crate::map`] (ground truth `erl_map.h:139-197`):
    /// head-array `2 + 16`, bitmap node `slots_offset + popcount(bitmap)`, collision
    /// node `1 + cells`. Deliberately a *different* formulation from
    /// `size_object`'s `1 + nslots + header_arity` worklist, so the two do not share
    /// a bug.
    fn walk_hamt(t: Eterm, is_head: bool, out: &mut HamtWalk) {
        let p = boxed_val(t);
        // SAFETY: `t` is a live node of the map being walked.
        let header = unsafe { *p };
        let (slots_off, nslots) = match map_header_type(header) as usize {
            MAP_HEADER_TAG_HAMT_HEAD_ARRAY => {
                assert!(is_head, "an array node can only be the head");
                out.head_array = true;
                (2, 16)
            }
            MAP_HEADER_TAG_HAMT_HEAD_BITMAP => {
                assert!(is_head, "a head-bitmap node can only be the head");
                out.head_bitmap = true;
                (2, map_header_val(header).count_ones() as usize)
            }
            MAP_HEADER_TAG_HAMT_NODE_BITMAP => {
                assert!(!is_head, "the head node is never an inner node");
                out.inner_nodes += 1;
                (1, map_header_val(header).count_ones() as usize)
            }
            other => panic!("not a HAMT node header (map tag {other})"),
        };
        out.node_words += slots_off + nslots;
        for i in 0..nslots {
            // SAFETY: slot `i` is within the node's payload.
            let slot = unsafe { *p.add(slots_off + i) };
            assert!(!is_immediate(slot), "a HAMT slot never holds an immediate");
            if is_list(slot) {
                out.leaves += 1;
            } else {
                // SAFETY: a non-leaf slot is a live boxed sub-node or collision tuple.
                let sub = unsafe { *boxed_val(slot) };
                if is_map_header(sub) {
                    walk_hamt(slot, false, out);
                } else {
                    assert!(is_arity_value(sub), "a collision node is a tuple");
                    let cells = arityval(sub);
                    out.collision_nodes += 1;
                    out.node_words += 1 + cells;
                    out.leaves += cells;
                }
            }
        }
    }

    /// The expected `size_object` of a hash map, computed by [`walk_hamt`].
    fn expected_hashmap_words(map: Eterm) -> HamtWalk {
        let mut out = HamtWalk::default();
        walk_hamt(map, true, &mut out);
        out
    }

    #[test]
    fn flatmap_sizes_at_the_hashmap_boundary_are_hand_computed() {
        // A non-empty flat map of `n` pairs is `2n + 4` words: the
        // `[arityval(n), keys…, head, n, keys-tuple, values…]` block (`4 + 2n`,
        // `thing::flatmap_size_words`) counted as its `n + 3`-word body plus the
        // `n + 1`-word keys tuple. The empty map is the special 3-word
        // `[head, 0, tuple0()]` block (the zero-arity tuple is a shared literal).
        for (n, expected) in [(0usize, 3usize), (1, 6), (2, 8), (31, 66), (32, 68)] {
            let b = build_map(&int_pairs(n as i64));
            assert!(is_flatmap_header(unsafe { *boxed_val(b.map) }));
            assert_eq!(size_object(b.map), expected, "flat map of {n} keys");
        }
        // 33 keys is the smallest hash map: its size follows the trie instead.
        let hash = build_map(&int_pairs(33));
        assert!(!is_flatmap_header(unsafe { *boxed_val(hash.map) }));
        let walk = expected_hashmap_words(hash.map);
        assert_eq!(size_object(hash.map), walk.total());
        assert_ne!(size_object(hash.map), 2 * 33 + 4);
    }

    #[test]
    fn hashmap_trie_is_counted_in_full() {
        for n in [33i64, 40, 100, 500] {
            let b = build_map(&int_pairs(n));
            assert!(
                !is_flatmap_header(unsafe { *boxed_val(b.map) }),
                "{n} keys must be a hash map"
            );
            let walk = expected_hashmap_words(b.map);
            assert!(
                walk.head_array || walk.head_bitmap,
                "the head is a HAMT node"
            );
            assert_eq!(walk.leaves, n as usize, "one leaf cell per key");
            assert_eq!(
                size_object(b.map),
                walk.total(),
                "every HAMT node and leaf of the {n}-key map must be counted"
            );
        }
    }

    #[test]
    fn hashmap_regression_is_no_longer_the_head_only_under_count() {
        let b = build_map(&int_pairs(500));
        let walk = expected_hashmap_words(b.map);
        assert!(
            walk.inner_nodes > 0,
            "a 500-key map must contain inner HAMT node-bitmap nodes"
        );
        assert_eq!(size_object(b.map), walk.total());
        // The figure the previous dispatch returned: the head node's own words
        // only (`1 + popcount(head bitmap) + head arity`, at most `18`), with no
        // descent into the trie at all.
        let head = unsafe { *boxed_val(b.map) };
        let old_head_only = 1 + map_header_val(head).count_ones() as usize + header_arity(head);
        assert!(old_head_only <= 18);
        assert!(
            size_object(b.map) > old_head_only,
            "the 500-key map is reported as {old_head_only} words (the old under-count)"
        );
    }

    #[test]
    fn every_hashmap_node_kind_is_counted() {
        // (a) Head bitmap holding two leaf slots:
        //     `[hdr][size][slot0][slot1]` = 4 node words + 2 * 2 leaf words = 8.
        let mut heap = Heap::new(32);
        let l0 = leaf(&mut heap, 1);
        let l1 = leaf(&mut heap, 2);
        // SAFETY: 4 words reserved for the head node.
        let head = unsafe { heap.alloc_words(4).unwrap() };
        unsafe {
            *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b0011);
            *head.add(1) = Eterm::from_raw(2); // the head's key-count `size` word
            *head.add(2) = l0;
            *head.add(3) = l1;
        }
        let map = make_boxed(head);
        assert_eq!(size_object(map), 8);
        let walk = expected_hashmap_words(map);
        assert!(walk.head_bitmap && walk.inner_nodes == 0 && walk.collision_nodes == 0);
        assert_eq!(walk.total(), 8);

        // (b) Head array: `[hdr][size][16 slots]` = 18 node words + 16 leaves.
        let mut heap = Heap::new(64);
        let leaves: Vec<Eterm> = (0..16).map(|k| leaf(&mut heap, k)).collect();
        // SAFETY: 18 words reserved for the head-array node.
        let head = unsafe { heap.alloc_words(18).unwrap() };
        unsafe {
            *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff);
            *head.add(1) = Eterm::from_raw(16);
            for (i, cell) in leaves.iter().enumerate() {
                *head.add(2 + i) = *cell;
            }
        }
        let map = make_boxed(head);
        assert_eq!(size_object(map), 18 + 16 * 2);
        let walk = expected_hashmap_words(map);
        assert!(walk.head_array && walk.leaves == 16);
        assert_eq!(walk.total(), 18 + 16 * 2);

        // (c) Head bitmap -> inner node bitmap -> collision tuple of two cells:
        //     head 3 + inner 2 + tuple 3 + 2 * 2 leaf words = 12.
        let mut heap = Heap::new(32);
        let c0 = leaf(&mut heap, 1);
        let c1 = leaf(&mut heap, 2);
        // SAFETY: 3 words reserved for the collision-node tuple.
        let coll = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *coll = make_arityval(2);
            *coll.add(1) = c0;
            *coll.add(2) = c1;
        }
        // SAFETY: 2 words reserved for the inner node bitmap.
        let inner = unsafe { heap.alloc_words(2).unwrap() };
        unsafe {
            *inner = make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, 0b0001);
            *inner.add(1) = make_boxed(coll);
        }
        // SAFETY: 3 words reserved for the head node.
        let head = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b1000);
            *head.add(1) = Eterm::from_raw(2);
            *head.add(2) = make_boxed(inner);
        }
        let map = make_boxed(head);
        assert_eq!(size_object(map), 12);
        let walk = expected_hashmap_words(map);
        assert!(walk.head_bitmap && walk.inner_nodes == 1 && walk.collision_nodes == 1);
        assert_eq!(walk.total(), 12);
    }

    #[test]
    fn refc_binary_counts_the_whole_bin_ref_pair() {
        // A refc binary — an `ErlSubBits` whose `orig` is a `BinRef` — is the 8-word
        // on-heap pair `[BinRef(3)][ErlSubBits(5)]`; `size_object_x` counts the whole
        // pair in one go (`copy.c:224-231`). The system ERTS agrees:
        // `erts_debug:flat_size(binary:copy(<<0>>, 100)) == 8`.
        #[repr(align(8))]
        struct Payload([u8; 8]);
        let payload = Payload([1, 2, 3, 4, 5, 6, 7, 8]);
        let mut heap = Heap::new(16);
        // SAFETY: 8 words reserved for the pair; `payload` outlives the term.
        let term = unsafe {
            let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
            write_refc_binary(
                hp,
                payload.0.as_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                payload.0.as_ptr(),
                0,
                64,
            )
        };
        assert_eq!(size_object(term), 8);
        // Reached through a composite term the pair is still counted exactly once.
        let cell = heap.cons(term, NIL).unwrap();
        assert_eq!(size_object(cell), 2 + 8);
    }

    #[test]
    fn sub_bits_match_context_counts_the_copied_heap_bits() {
        // A match context whose `orig` is an on-heap bitstring: the copy
        // materialises a fresh `ErlHeapBits` block for the window rather than the
        // match context itself (`copy.c:232-239`).
        let mut heap = Heap::new(64);
        // SAFETY: 3 words for a 64-bit (8-byte) heap bitstring.
        let base = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(64)).unwrap();
            write_heap_bits(hp, &[0xAA; 8], 64).unwrap()
        };
        assert_eq!(size_object(base), 3);
        // `heap_bits_size(bits) == 2 + ceil(bits / 64)`.
        for (bits, expected) in [(9usize, 3usize), (64, 3), (65, 4), (520, 11)] {
            // SAFETY: 5 words for the `ErlSubBits`; `base` is a live heap bitstring.
            let sub = unsafe {
                let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
                write_sub_bits(
                    hp,
                    Eterm::from_raw(0),
                    Eterm::from_raw(0),
                    Eterm::from_raw(bits),
                    base,
                )
            };
            assert_eq!(
                size_object(sub),
                expected,
                "{bits}-bit match context window"
            );
        }
    }

    #[test]
    fn sub_bits_without_a_boxed_origin_reports_its_thing_size() {
        // A non-boxed `orig` is not a term the C accepts: `copy.c:222` dereferences
        // `boxed_val(sb->orig)` unconditionally. Report the thing size instead of
        // following a bogus pointer.
        let mut heap = Heap::new(8);
        // SAFETY: 5 words reserved for the `ErlSubBits`.
        let sub = unsafe {
            let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                make_small(3),
            )
        };
        assert_eq!(size_object(sub), ERL_SUB_BITS_SIZE);
    }
}
