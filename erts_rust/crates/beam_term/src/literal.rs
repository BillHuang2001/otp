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

//! Off-heap literal areas (`TAG_LITERAL_PTR`).
//!
//! Ground truth: `erts_set_literal_tag` in `erts/emulator/beam/erl_term.c`. A
//! literal area is a bump-allocated, never-reallocated block of term words whose
//! boxed/list words carry the `TAG_LITERAL_PTR` bit so the runtime can tell them
//! apart from heap terms.

use core::mem::MaybeUninit;

use crate::boxed::{header_is_thing, thing_arityval, HEADER_SUB_BITS, is_boxed};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::list::is_list;
use crate::tag::{primary_tag, PrimaryTag, TAG_LITERAL_PTR};

/// `is_literal_ptr(x)` — whether the literal-area bit is set on a tagged pointer.
///
/// Mirrors the C macro exactly: it is a plain bit test, so it is only meaningful
/// when applied to a BOXED or LIST word.
#[inline]
pub const fn is_literal(x: Eterm) -> bool {
    (x.raw() & TAG_LITERAL_PTR) != 0
}

/// Mark a tagged pointer as belonging to a literal area.
#[inline]
pub const fn make_literal(x: Eterm) -> Eterm {
    Eterm::from_raw(x.raw() | TAG_LITERAL_PTR)
}

/// Remove the literal-area bit from a tagged pointer.
#[inline]
pub const fn clear_literal(x: Eterm) -> Eterm {
    Eterm::from_raw(x.raw() & !TAG_LITERAL_PTR)
}

/// OR the literal bit into every BOXED/LIST word of a contiguous word range.
///
/// The shared walk behind both [`set_literal_tag`] (raw pointers) and
/// [`tag_literal_area`] (safe slices). It is index-based, so it is itself safe.
fn tag_words(area: &mut [Eterm]) {
    let mut i = 0;
    while i < area.len() {
        let word = area[i];
        match primary_tag(word) {
            PrimaryTag::Boxed | PrimaryTag::List => {
                area[i] = Eterm::from_raw(word.raw() | TAG_LITERAL_PTR);
            }
            PrimaryTag::Header => {
                if word == HEADER_SUB_BITS {
                    // Tag the `orig` field as a literal. It is the last field
                    // inside the thing structure, so we can handle it by
                    // pretending it is not part of the thing.
                    i += thing_arityval(word) - 1;
                } else if header_is_thing(word) {
                    i += thing_arityval(word);
                }
            }
            PrimaryTag::Immed1 => {}
        }
        i += 1;
    }
}

/// `erts_set_literal_tag(term, hp_start, hsz)` — walk a term graph and OR
/// `TAG_LITERAL_PTR` into every BOXED/LIST word and into the root `*term`.
///
/// The safe, slice-based equivalent is [`tag_literal_area`].
///
/// # Safety
///
/// `hp_start` must point at `hsz` initialised, valid term words forming a
/// well-formed term graph (thing headers must cover their bodies), and `term`
/// must be a live, aligned, writable term word.
pub unsafe fn set_literal_tag(term: *mut Eterm, hp_start: *mut Eterm, hsz: usize) {
    // SAFETY: the caller guarantees `hp_start`/`term` are valid, aligned,
    // initialised, writable term words; the slices do not outlive this call.
    let area = unsafe { core::slice::from_raw_parts_mut(hp_start, hsz) };
    let roots = unsafe { core::slice::from_raw_parts_mut(term, 1) };
    tag_literal_area(area, roots);
}

/// The **safe** form of [`set_literal_tag`]: a slice-based literal-tagging walk.
///
/// `area` is a contiguous literal arena; `roots` is the set of root term words
/// that point into it (in C, the single `*term`). This reproduces the
/// `erts_set_literal_tag` walk verbatim
/// (`../../erts/emulator/beam/erl_term.c::erts_set_literal_tag`) — the same
/// word-by-word rule set, applied to `area` and then to each root — but with no
/// raw pointer and no `unsafe` of its own.
///
/// Rule-by-rule against the C:
///
/// * **BOXED / LIST words** (`TAG_PRIMARY_BOXED`/`TAG_PRIMARY_LIST`) are ORed
///   with `TAG_LITERAL_PTR = 0x4` (bit 2). That bit sits *above* the 2-bit
///   primary tag (bits 0..1) so the word stays a BOXED/LIST word, and *inside*
///   the 3-bit pointer tag (`TAG_PTR_MASK = 0x7`) that `ptr_val` already masks
///   off — so `list_val`/`boxed_val` still resolve the same address, only now
///   marked as living in the literal area.
/// * **HEADERS** (`TAG_PRIMARY_HEADER`) are *never* tagged. The walk skips the
///   header and its body: ORing bit 2 here would corrupt the header's subtag,
///   so instead the header word and the words it covers are stepped over.
///   * The `HEADER_SUB_BITS` special case, verbatim from the C:
///     `Tag the 'orig' field as a literal. It's the last field inside the thing
///     structure so we can handle it by pretending it's not part of the thing.`
///     i.e. advance by `thing_arityval(word) - 1` words, leaving the walk on the
///     `orig` field (the last thing word) so the *following* iteration tags it
///     as a pointer.
///   * Otherwise, if `header_is_thing(word)` (a genuine thing header, not a
///     transparent arityval/record), advance by its `thing_arityval(word)` body
///     words so the whole thing body is left intact.
/// * **IMMED1 words** (`TAG_PRIMARY_IMMED1`: small ints, atoms, `NIL`, …) are
///   left untouched — they carry no pointer and no literal bit.
///
/// After the walk, each root in `roots` that `is_boxed` or `is_list` is ORed
/// with `TAG_LITERAL_PTR` (the C's trailing `if (is_boxed(*term) ||
/// is_list(*term)) *term |= TAG_LITERAL_PTR;`).
///
/// A malformed arena cannot panic: the walk only ever advances the index
/// (bounded by the loop condition) and never re-reads out of bounds.
pub fn tag_literal_area(area: &mut [Eterm], roots: &mut [Eterm]) {
    tag_words(area);

    for root in roots.iter_mut() {
        if is_boxed(*root) || is_list(*root) {
            *root = Eterm::from_raw(root.raw() | TAG_LITERAL_PTR);
        }
    }
}

/// A bump-allocated, frozen literal term block.
///
/// Never reallocates, so handed-out pointers stay valid for the area's lifetime.
pub struct LiteralArea {
    storage: Box<[MaybeUninit<Eterm>]>,
    used: usize,
}

impl LiteralArea {
    /// A new area of `capacity_words` term words.
    pub fn new(capacity_words: usize) -> LiteralArea {
        LiteralArea {
            storage: vec![MaybeUninit::uninit(); capacity_words].into_boxed_slice(),
            used: 0,
        }
    }

    /// Total capacity in words.
    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    /// Words handed out so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Words still available.
    pub fn available(&self) -> usize {
        self.storage.len() - self.used
    }

    /// Rewind the bump pointer (the block is expected to be frozen first).
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// First word of the block.
    pub fn start(&self) -> *mut Eterm {
        self.storage.as_ptr() as *mut Eterm
    }

    /// One past the last word of the block.
    pub fn end(&self) -> *mut Eterm {
        unsafe { (self.storage.as_ptr() as *mut Eterm).add(self.storage.len()) }
    }

    /// Allocate `n` contiguous, **uninitialised** words.
    ///
    /// # Safety
    ///
    /// The returned words are uninitialised; the caller must write them before
    /// they are read, and must not use them after this area is dropped.
    pub unsafe fn alloc_words(&mut self, n: usize) -> TermResult<*mut Eterm> {
        if n > self.available() {
            return Err(TermError::HeapExhausted);
        }
        let p = unsafe { self.start().add(self.used) };
        self.used += n;
        Ok(p)
    }

    /// Whether `ptr` points inside this block.
    pub fn contains(&self, ptr: *const Eterm) -> bool {
        let p = ptr as usize;
        let start = self.start() as usize;
        let end = self.end() as usize;
        p >= start && p < end
    }

    /// Pointer-range test: whether `ptr` lives in this literal block.
    pub fn is_literal(&self, ptr: *const Eterm) -> bool {
        self.contains(ptr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::make_arityval;
    use crate::immediate::{make_small, NIL};
    use crate::list::{car, cdr, list_val, make_list};
    use crate::tuple::make_tuple;

    #[test]
    fn literal_bit_helpers() {
        let t = Eterm::from_raw(0x1000 | 0x2);
        assert!(!is_literal(t));
        assert!(is_literal(make_literal(t)));
        assert_eq!(clear_literal(make_literal(t)), t);
    }

    #[test]
    fn set_literal_tag_walks_nested_tuple_and_list() {
        // tuple(2) = { 1, [2 | []] }
        let mut words = [NIL; 5];
        words[0] = make_arityval(2);
        words[1] = make_small(1);
        let cons_ptr = unsafe { words.as_ptr().add(3) };
        words[2] = make_list(cons_ptr);
        words[3] = make_small(2);
        words[4] = NIL;
        let mut root = make_tuple(words.as_mut_ptr());

        unsafe { set_literal_tag(&mut root, words.as_mut_ptr(), 5) };

        assert!(is_literal(root), "root tuple must be tagged");
        assert!(is_literal(words[2]), "cons cell must be tagged");
        // Non-pointer words are untouched. (`is_literal` is a bare bit test, so it
        // is only meaningful for tagged pointers; compare the words themselves.)
        assert_eq!(words[0], make_arityval(2), "arityval header is transparent");
        assert_eq!(words[1], make_small(1), "small int is an immediate");
        assert_eq!(words[4], NIL, "NIL is an immediate");

        // the tagging must not disturb the graph
        unsafe {
            let list = words[2];
            assert_eq!(car(list), make_small(2));
            assert_eq!(cdr(list), NIL);
            assert_eq!(list_val(list) as usize, words.as_ptr().add(3) as usize);
        }
    }

    #[test]
    fn set_literal_tag_descends_into_sub_bits_orig() {
        // sub-bits thing whose `orig` is a boxed pointer to a further thing.
        let mut words = [NIL; 7];
        words[0] = HEADER_SUB_BITS; // arity 4
        words[1] = Eterm::from_raw(0); // base_flags
        words[2] = Eterm::from_raw(0); // start
        words[3] = Eterm::from_raw(16); // end
        let orig_ptr = unsafe { words.as_ptr().add(5) };
        words[4] = crate::boxed::make_boxed(orig_ptr); // orig
        words[5] = make_arityval(1);
        words[6] = make_small(99);
        let mut root = crate::boxed::make_boxed(words.as_ptr());

        unsafe { set_literal_tag(&mut root, words.as_mut_ptr(), 7) };

        assert!(is_literal(root));
        assert!(is_literal(words[4]), "orig field must be tagged");
        assert_eq!(
            crate::boxed::boxed_val(words[4]) as usize,
            unsafe { words.as_ptr().add(5) } as usize
        );
        assert!(!is_literal(words[5]));
    }

    #[test]
    fn tag_literal_area_walks_nested_tuple_and_list() {
        // tuple(2) = { 1, [2 | []] }, driven through the safe slice API.
        let mut words = [NIL; 5];
        words[0] = make_arityval(2);
        words[1] = make_small(1);
        let cons_ptr = unsafe { words.as_ptr().add(3) };
        words[2] = make_list(cons_ptr);
        words[3] = make_small(2);
        words[4] = NIL;
        let mut root = make_tuple(words.as_mut_ptr());

        let root_ptr = words.as_ptr();
        tag_literal_area(&mut words, core::slice::from_mut(&mut root));

        assert!(is_literal(root), "root tuple must be tagged");
        assert!(is_literal(words[2]), "cons cell must be tagged");
        // Non-pointer words are untouched (compare the words themselves, since
        // `is_literal` is a bare bit test meaningful only for tagged pointers).
        assert_eq!(words[0], make_arityval(2), "arityval header is transparent");
        assert_eq!(words[1], make_small(1), "small int is an immediate");
        assert_eq!(words[4], NIL, "NIL is an immediate");

        // The tagging must not disturb the graph: the tagged cons cell still
        // resolves to the same address and the same CAR/CDR.
        unsafe {
            let list = words[2];
            assert_eq!(car(list), make_small(2));
            assert_eq!(cdr(list), NIL);
            assert_eq!(
                list_val(list) as *const Eterm,
                root_ptr.add(3),
                "list_val masks the literal bit, so the pointer is unchanged"
            );
        }
    }

    #[test]
    fn tag_literal_area_descends_into_sub_bits_orig() {
        // sub-bits thing whose `orig` is a boxed pointer to a further thing.
        let mut words = [NIL; 7];
        words[0] = HEADER_SUB_BITS; // arity 4
        words[1] = Eterm::from_raw(0); // base_flags
        words[2] = Eterm::from_raw(0); // start
        words[3] = Eterm::from_raw(16); // end
        let orig_ptr = unsafe { words.as_ptr().add(5) };
        words[4] = crate::boxed::make_boxed(orig_ptr); // orig
        words[5] = make_arityval(1);
        words[6] = make_small(99);
        let mut root = crate::boxed::make_boxed(words.as_ptr());

        tag_literal_area(&mut words, core::slice::from_mut(&mut root));

        assert!(is_literal(root));
        assert!(is_literal(words[4]), "orig field must be tagged");
        assert_eq!(
            crate::boxed::boxed_val(words[4]) as usize,
            unsafe { words.as_ptr().add(5) } as usize
        );
        assert!(!is_literal(words[5]), "the word after orig is not a pointer");
    }

    #[test]
    fn tag_literal_area_empty_inputs_are_a_noop() {
        let mut empty: [Eterm; 0] = [];
        let mut roots: [Eterm; 0] = [];
        tag_literal_area(&mut empty, &mut roots);

        // Immediate roots are left bit-identical (no area at all).
        let mut roots = [make_small(7), NIL, make_small(-3)];
        tag_literal_area(&mut empty, &mut roots);
        assert_eq!(roots[0], make_small(7));
        assert_eq!(roots[1], NIL);
        assert_eq!(roots[2], make_small(-3));

        // An empty `roots` still tags the area; a transparent arityval header
        // is stepped over by the walk and left intact.
        let mut words = [make_arityval(1), make_small(3)];
        tag_literal_area(&mut words, &mut []);
        assert_eq!(words[0], make_arityval(1), "header stays transparent");
        assert_eq!(words[1], make_small(3));

        // Faithful-port pin: a HEADER-word *root* is `is_boxed` under the C bit
        // trick `(x & 0x1) == 0`, so the C (and this port) DO tag it. This is
        // only observable for a forged header root; a real literal root is a
        // boxed pointer or a cons cell.
        let mut header_root = [make_arityval(1)];
        tag_literal_area(&mut [], &mut header_root);
        assert!(is_literal(header_root[0]));
    }
    #[test]
    fn tag_literal_area_tags_every_root() {
        // Two roots pointing at the same arena; both must receive the literal bit.
        let mut words = [NIL; 3];
        let child_ptr = unsafe { words.as_mut_ptr().add(1) };
        words[0] = make_tuple(child_ptr); // boxed root
        words[1] = make_arityval(1);
        words[2] = make_small(5);
        let mut roots = [words[0], make_small(9)];

        tag_literal_area(&mut words, &mut roots);

        assert!(is_literal(words[0]));
        assert!(is_literal(roots[0]));
        assert_eq!(roots[1], make_small(9), "an immediate root is untouched");
    }
    #[test]
    fn literal_area_bump_and_range_checks() {
        let mut area = LiteralArea::new(8);
        assert_eq!(area.capacity(), 8);
        assert_eq!(area.available(), 8);
        assert_eq!(area.used(), 0);

        let p = unsafe { area.alloc_words(3).unwrap() };
        assert_eq!(area.used(), 3);
        assert_eq!(area.available(), 5);
        assert!(area.contains(p));
        assert!(area.is_literal(p));
        assert!(area.contains(unsafe { p.add(2) }));
        // `contains` is a whole-block range test, bounded by `end()`.
        assert!(area.contains(unsafe { p.add(3) }));
        assert!(!area.contains(area.end()));
        assert!(!area.contains(area.start().wrapping_sub(1)));

        assert_eq!(
            unsafe { area.alloc_words(6) },
            Err(TermError::HeapExhausted)
        );
        assert!(unsafe { area.alloc_words(5) }.is_ok());
        assert_eq!(area.available(), 0);

        area.reset();
        assert_eq!(area.used(), 0);
        assert_eq!(area.available(), 8);
    }

    #[test]
    fn literal_area_zero_capacity() {
        let area = LiteralArea::new(0);
        assert_eq!(area.capacity(), 0);
        assert!(!area.contains(area.start()));
    }
}
