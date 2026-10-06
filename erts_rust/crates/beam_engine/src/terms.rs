//! Boxed-term construction and reading behind the execution-context heap seam —
//! the engine's **second** `unsafe` module.
//!
//! The interpreter has to *build* boxed terms (`put_tuple2`, `put_list`, float
//! literals, `make_fun`, binary construction, …) and *read* them back
//! (`get_tuple_element`, `get_list`, map access, fun introspection). Term
//! construction/inspection itself is owned by `beam_term` (the frozen-layout
//! lowest common ancestor, workspace `CONTEXT.md` §2), but `beam_term` is a
//! dependency *sink*: it never allocates. This module is the glue that obtains raw
//! storage from [`ExecutionContext::alloc_words`] and passes it to the
//! `beam_term` writers — the fun header/descriptor layout is owned by
//! [`beam_term::fun`] and only the **heap binary** layout is hand-rolled here.
//!
//! # Ownership model
//!
//! * The **context owns the heap**. [`ExecutionContext::alloc_words`] hands out a
//!   stable pointer to `n` uninitialized words; those words stay valid and
//!   unmoved until the next [`ExecutionContext::ensure_heap`] call (a GC), exactly
//!   as the C `HAlloc`/`heap_top` convention. Nothing here frees or moves them.
//! * **Only this module dereferences** those raw pointers. It is the crate's — and
//!   the widened interpreter's — single point of raw-heap access, so the crate-wide
//!   `#![deny(unsafe_code)]` is lifted for it alone (alongside `predicates`) at the
//!   `mod` declaration in `lib.rs`.
//! * Every `unsafe` block carries a `// SAFETY:` rationale and every body is an
//!   explicit `unsafe {}` block, so `unsafe_op_in_unsafe_fn` stays clean.
//!
//! Reuse is deliberate: tuples, cons cells, floats, flat maps, bignums, the
//! external pid/port/ref blocks and **funs** ([`beam_term::fun`]) are built and read
//! by the matching `beam_term` primitive; only the **heap binary** family has no
//! `beam_term` owner and hand-rolls a layout here, pinned line-by-line to the C
//! ground truth in the function docs.
//!
//! The **hashmap (HAMT) read path** is hand-rolled here as well: `beam_term` owns
//! the frozen map-header tag constants (`HAMT_SUBTAG_*`) and the `is_hashmap`
//! classifier but no node reader, so [`map_entries`] walks the `erl_map.h` node
//! tree itself. No hash function is needed — the walk descends every occupied slot
//! and key matching stays with the caller's `beam_term::cmp` scan. No crate in the
//! rewrite can yet *build* a HAMT, so map construction ([`make_flatmap`]) stays
//! capped at the flat-map limit.
//!
//! # Error strategy
//!
//! Everything returns [`beam_common::Result`] (never a crate-local enum): a wrong
//! term type or an out-of-range index is [`BeamError::BadArg`], an unimplemented
//! case is [`crate::error::unsupported`], and a hard representation limit is
//! [`BeamError::SystemLimit`]. No function panics on runtime data.
//!
//! # Why this file is long
//!
//! It is the crate's single raw-heap boxed-term builder/reader (the widening
//! contract: one `#[allow(unsafe_code)]` home for every term layout), so tuples,
//! cons cells, lists, floats, flat maps, the HAMT reader, funs and the heap binary
//! all live here, each with the frozen-layout proof it needs — plus a comprehensive
//! in-module test suite (raw-word layout assertions and the HAMT fixtures). Moving
//! a family out would spread `unsafe` across modules and needs `lib.rs` wiring, so
//! the responsibility deliberately stays in one file.
use beam_common::{BeamError, Result};
use beam_loader::Module;
use beam_term::{make_atom, make_small, thing, Eterm, MAX_SMALL, MIN_SMALL};

use crate::context::ExecutionContext;

/// `MAP_SMALL_MAP_LIMIT` (`erl_map.h:93-97`): the largest flat-map pair count.
/// Above it ERTS uses a **hashmap** (HAMT). No crate in the rewrite can build one
/// yet (it belongs to `beam_term`/`beam_codec`), so [`make_flatmap`] rejects a
/// larger map — but [`map_entries`] can still *read* one.
const MAP_SMALL_MAP_LIMIT: usize = 32;

/// The word offset of a HAMT node's first item when the node carries the
/// `[header, size, items…]` prefix — `hashmap_head_t` (`erl_map.h:142-146`):
/// the head array (`HAMT_HEAD_ARRAY_SZ = 18`) and the head bitmap
/// (`HAMT_HEAD_BITMAP_SZ(n) = 2 + n`).
const HAMT_HEAD_ITEM_OFFSET: usize = 2;
/// The word offset of a HAMT node's first item when the node is header-only —
/// `HAMT_NODE_BITMAP_SZ(n) = 1 + n` and `HAMT_COLLISION_NODE_SZ(n) = 1 + n`
/// (`erl_map.h:203-205`).
const HAMT_NODE_ITEM_OFFSET: usize = 1;
/// A HAMT head **array** node always holds all 16 slots (`HAMT_NODE_ARRAY_SZ`,
/// `erl_map.h:201`; ERTS only promotes a bitmap head to an array head once every
/// slot is occupied, `erl_map.c:2755-2757`).
const HAMT_ARRAY_SLOTS: usize = 16;
/// `ERL_ONHEAP_BINARY_LIMIT` (`erl_bits.h:167`): the largest byte count a hardware
/// *heap* binary holds; a larger binary must be off-heap (a ref-counted binary),
/// which this module does not build.
const ERL_ONHEAP_BINARY_LIMIT: usize = 64;
/// `sizeof(ErlHeapBits)/sizeof(Eterm) - 1` (`erl_bits.h:156-158`): the fixed head
/// (`thing_word`, `size`) that precedes the binary data words.
const HEAP_BIN_HEAD_WORDS: usize = 2;
/// Number of bytes in one term word.
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();

// ---------------------------------------------------------------------------
// Constructors (take the execution-context heap seam)
// ---------------------------------------------------------------------------

/// Builds a tuple term from `elems`.
///
/// Allocates `1 + elems.len()` words: an [`arityval`](beam_term::make_arityval)
/// header followed by the elements (the `make_tuple`/`tuple_val` layout of
/// `beam_term::tuple`). An empty slice returns the shared zero-arity literal
/// [`tuple0`](beam_term::tuple0) (C `ERTS_GLOBAL_LIT_EMPTY_TUPLE`).
///
/// More than `MAX_ARITYVAL` (`(1 << 24) - 1`) elements yields
/// [`BeamError::SystemLimit`].
pub fn make_tuple(ctx: &mut dyn ExecutionContext, elems: &[Eterm]) -> Result<Eterm> {
    if elems.is_empty() {
        return Ok(beam_term::tuple0());
    }
    if elems.len() > beam_term::MAX_ARITYVAL {
        return Err(BeamError::SystemLimit);
    }
    let hp = ctx.alloc_words(1 + elems.len())?;
    // SAFETY: `alloc_words` returned `1 + n` exclusively-owned, writable words;
    // word 0 receives the arityval header and words `1..=n` the elements.
    let root = unsafe {
        *hp = beam_term::make_arityval(elems.len());
        for (i, elem) in elems.iter().enumerate() {
            *hp.add(1 + i) = *elem;
        }
        hp
    };
    Ok(beam_term::make_tuple(root))
}

/// Builds a single cons cell `[Head | Tail]`.
///
/// Allocates two words and writes the header-less `[CAR, CDR]` cell tagged
/// `TAG_PRIMARY_LIST` (via [`make_list`](beam_term::make_list)) — exactly the
/// `put_list` layout of `emu/instrs.tab:732-737` (`HTOP[0] = $Hd; HTOP[1] = $Tl;
/// $Dst = make_list(HTOP)`). `tail` need **not** be a list: `[H | T]` with an
/// improper `T` is a legal Erlang term, so no shape check is performed here.
pub fn make_cons(ctx: &mut dyn ExecutionContext, head: Eterm, tail: Eterm) -> Result<Eterm> {
    let hp = ctx.alloc_words(2)?;
    // SAFETY: `alloc_words` returned 2 exclusively-owned, writable words — one
    // `[CAR, CDR]` cell; `make_list` tags the pointer as a list term.
    unsafe {
        *hp = head;
        *hp.add(1) = tail;
    }
    Ok(beam_term::make_list(hp))
}

/// Builds a proper list term from `elems`.
///
/// One `[CAR, CDR]` cell per element, linked front-to-back by [`make_cons`], the
/// final `CDR` being [`NIL`](beam_term::NIL). An empty slice returns `NIL`. The
/// cells are individually allocated — the C `make_list` allocates `2n` contiguous
/// words, but contiguity is *not* part of the frozen term layout, so building on
/// [`make_cons`] is equivalent.
pub fn make_list(ctx: &mut dyn ExecutionContext, elems: &[Eterm]) -> Result<Eterm> {
    let mut list = beam_term::NIL;
    for elem in elems.iter().rev() {
        list = make_cons(ctx, *elem, list)?;
    }
    Ok(list)
}

/// The `{badmatch, Value}` exception reason (class `error`) for a failed
/// tuple/list match.
///
/// ERTS tags the reason with the **global** atom `badmatch` (`am_badmatch`,
/// `beam_common.c:388`; the C `badmatch`/`get_*` opcodes store `freason = BADMATCH`
/// and the tag is materialized during error handling). This engine's atoms are
/// **module-local** indices (the frozen term model, `../CONTEXT.md` §2), so the tag
/// is resolved through `module`'s atom table — the only table whose indices the
/// loaded code compares against. The Erlang compiler never interns `badmatch` (the
/// raising opcode uses the C global constant), so a real module normally lacks it;
/// minting an arbitrary atom index there would be a *wrong result*, so this returns
/// the canonical typed [`BeamError::BadMatch`] instead.
pub fn badmatch_reason(
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    value: Eterm,
) -> Result<Eterm> {
    let tag = module
        .atom_table
        .lookup("badmatch")
        .ok_or(BeamError::BadMatch)?;
    make_tuple(ctx, &[make_atom(tag), value])
}

/// Builds a float term from `value`.
///
/// The `[HEADER_FLONUM, f64 bits]` layout (2 words) is written by
/// [`beam_term::thing::write_float`]. The pointer is valid until the next
/// [`ExecutionContext::ensure_heap`] call (the documented stability contract).
pub fn make_float(ctx: &mut dyn ExecutionContext, value: f64) -> Result<Eterm> {
    let hp = ctx.alloc_words(thing::FLOAT_SIZE_WORDS)?;
    // SAFETY: `alloc_words` returned exactly `FLOAT_SIZE_WORDS` writable words,
    // which `write_float` fully initialises.
    Ok(unsafe { thing::write_float(hp, value) })
}

/// Builds an integer term for `value`: a small int when it fits, an exact bignum
/// otherwise.
///
/// `MIN_SMALL..=MAX_SMALL` (the 60-bit immediate range, `beam_term::immediate`) is
/// materialised with [`beam_term::make_small`]. Anything outside it — reachable from
/// real `erlc` output, whose `integer`/`character` operands may carry a value in
/// `[2^59, 2^63)` — is written as an **exact bignum** with
/// [`beam_term::thing::write_big`] (`BIG_NEED_SIZE(1) = 2` words: the header plus one
/// `ErtsDigit`, with the sign in the header's `_BIG_SIGN_BIT`). A non-small `i64`
/// always fits exactly one `u64` `ErtsDigit`, so the digit slice is normalised
/// (non-empty, no leading zero) by construction.
///
/// The pointer is valid until the next [`ExecutionContext::ensure_heap`] call (the
/// documented stability contract). This is the shared constructor
/// [`crate::operand::read_operand`] uses, so a small-vs-bignum mismatch is
/// impossible.
pub fn make_integer(ctx: &mut dyn ExecutionContext, value: i64) -> Result<Eterm> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        return Ok(make_small(value));
    }
    // `unsigned_abs` is total, including `i64::MIN`; a non-small value has a non-zero
    // magnitude, so there is no leading zero digit to strip.
    let digits = [value.unsigned_abs()];
    let hp = ctx.alloc_words(beam_term::big_size_words(digits.len()))?;
    // SAFETY: `alloc_words` returned exactly `big_size_words(1) = 2` exclusively-owned,
    // writable words — the block `write_big` requires and fully writes (header + one
    // `ErtsDigit`). The context owns the arena and keeps it live for as long as any
    // term built from this pointer is used.
    Ok(unsafe { thing::write_big(hp, value < 0, &digits) })
}

/// Builds a small *flat* map term from `pairs` (insertion order preserved).
///
/// The `[arityval(n), k0 … k(n-1), head, n, keys, v0 … v(n-1)]` layout is written
/// by [`beam_term::thing::write_flatmap`]. Duplicate keys are not detected here
/// (that needs term comparison).
///
/// More than `MAP_SMALL_MAP_LIMIT` (32) pairs requires a **hashmap** and a HAMT
/// builder. No crate in the rewrite implements one (`beam_term` — the frozen-layout
/// LCA — owns no HAMT builder, and `beam_codec` has neither a builder nor a HAMT
/// decoder), so a larger map yields [`crate::error::unsupported`] rather than a
/// wrong term. This is the documented construction-side gap; the read side
/// ([`map_entries`]) *does* handle a HAMT.
pub fn make_flatmap(ctx: &mut dyn ExecutionContext, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let n = pairs.len();
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(crate::error::unsupported("map > 32 pairs (no HAMT builder)"));
    }
    let hp = ctx.alloc_words(thing::flatmap_size_words(n))?;
    // SAFETY: `alloc_words` returned `flatmap_size_words(n)` writable words and
    // `n <= MAP_SMALL_MAP_LIMIT`, the preconditions `write_flatmap` documents.
    Ok(unsafe { thing::write_flatmap(hp, pairs) })
}

/// Builds a local **fun** (closure) term.
///
/// # Frozen word layout
///
/// The on-heap header/descriptor layout is owned by [`beam_term::fun`] (the
/// frozen-layout lowest common ancestor); this function only obtains the storage
/// from the execution context and writes it through that module's
/// [`make_fun_header`](beam_term::fun::make_fun_header) and
/// [`FUN_DESC_WORDS_LOCAL`](beam_term::fun::FUN_DESC_WORDS_LOCAL).
///
/// C's `ErlFunThing` (`erl_fun.h:65-99`) is
/// `[thing_word, entry, env[0..num_free]]`, where `entry` is a **pointer** to an
/// out-of-line `ErlFunEntry`/`Export` that owns the code address, `uniq[16]` and
/// `index` (`erl_fun.c`, `emu/instrs.tab:545-584` `make_fun3`).
///
/// M1 has no code addresses or fun-entry table, so the single `entry` pointer is
/// **inlined as two descriptor words** — the
/// [`FunKind::Local`](beam_term::fun::FunKind::Local) substitution
/// [`beam_term::fun`] owns. The resulting block is
/// `1 (header) + 2 (uniq, index) + num_free` words:
///
/// ```text
/// word 0            MAKE_FUN_HEADER(arity, num_free, Local)  (exact C encoding)
/// word 1            uniq   (u64, the fun's identity)  — replaces the entry pointer
/// word 2            index  (u32, the fun's lambda-table index)
/// words 3 .. 3+n    env[0..num_free]                  (identical to C's order/position)
/// ```
///
/// `arity` and `num_free` must each fit 8 bits; a larger value yields
/// [`BeamError::SystemLimit`]. The pointer is valid until the next
/// [`ExecutionContext::ensure_heap`] call.
pub fn make_fun(
    ctx: &mut dyn ExecutionContext,
    arity: u32,
    uniq: u64,
    index: u32,
    free: &[Eterm],
) -> Result<Eterm> {
    let num_free = free.len();
    if arity as usize > beam_term::fun::FUN_MAX_FIELD || num_free > beam_term::fun::FUN_MAX_FIELD {
        return Err(BeamError::SystemLimit);
    }
    let header =
        beam_term::fun::make_fun_header(arity as usize, num_free, beam_term::fun::FunKind::Local)
            .map_err(BeamError::from)?;
    let env_offset = 1 + beam_term::fun::FUN_DESC_WORDS_LOCAL;
    let hp = ctx.alloc_words(env_offset + num_free)?;
    // SAFETY: `alloc_words` returned `1 + FUN_DESC_WORDS_LOCAL + num_free`
    // exclusively-owned, writable words. Word 0 receives the fun header, word 1 the
    // `uniq`, word 2 the `index` and words `env_offset..env_offset + num_free` the
    // free variables — every write is in bounds.
    let root = unsafe {
        *hp = header;
        *hp.add(1) = Eterm::from_raw(uniq as usize);
        *hp.add(2) = Eterm::from_raw(index as usize);
        for (i, var) in free.iter().enumerate() {
            *hp.add(env_offset + i) = *var;
        }
        hp
    };
    Ok(beam_term::make_boxed(root))
}

/// Builds an on-heap binary term from `bytes`.
///
/// # Frozen word layout
///
/// `ErlHeapBits` (`erl_bits.h:149-164`) is `[thing_word, size, data…]`, where the
/// header is `_make_header(heap_bits_size(num_bits) - 1, HEAP_BITS_SUBTAG)` and
/// `size` is the **bit** count; the data words follow at word 2 (the block is
/// `2 + ceil(num_bytes / word_bytes)` words, `erl_bits.h:156-158`).
///
/// Only *heap* binaries (at most `ERL_ONHEAP_BINARY_LIMIT`, 64, bytes) are built;
/// a longer binary needs the off-heap ref-counted representation, which is not
/// implemented and yields [`crate::error::unsupported`].
pub fn make_heap_binary(ctx: &mut dyn ExecutionContext, bytes: &[u8]) -> Result<Eterm> {
    let num_bytes = bytes.len();
    if num_bytes > ERL_ONHEAP_BINARY_LIMIT {
        return Err(crate::error::unsupported("heap binary > 64 bytes"));
    }
    let data_words = num_bytes.div_ceil(WORD_BYTES);
    let num_words = HEAP_BIN_HEAD_WORDS + data_words;
    let hp = ctx.alloc_words(num_words)?;
    // SAFETY: `alloc_words` returned `num_words = 2 + ceil(num_bytes / word_bytes)`
    // exclusively-owned, writable words. Word 0 receives the heap-bits header (the
    // block size minus the header word), word 1 the bit count and the data section
    // at word 2 has room for `num_bytes` bytes.
    unsafe {
        *hp = beam_term::make_header(num_words - 1, beam_term::HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw((num_bytes * 8) as usize);
        let data = hp.add(HEAP_BIN_HEAD_WORDS).cast::<u8>();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, num_bytes);
    }
    Ok(beam_term::make_boxed(hp))
}

// ---------------------------------------------------------------------------
// Readers (no context needed; wrong types are `BeamError::BadArg`)
// ---------------------------------------------------------------------------

/// The arity of a tuple term; a non-tuple yields [`BeamError::BadArg`].
///
/// The exact inverse of [`make_tuple`] (`beam_term::tuple_size`).
pub fn tuple_arity(t: Eterm) -> Result<usize> {
    // SAFETY: `is_tuple` checks `is_boxed` before dereferencing the header word.
    if !unsafe { beam_term::is_tuple(t) } {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_tuple` established that `t` is a live tuple.
    Ok(unsafe { beam_term::tuple_size(t) })
}

/// The zero-based `i`-th element of a tuple; an out-of-range index or a non-tuple
/// yields [`BeamError::BadArg`].
pub fn tuple_element(t: Eterm, i: usize) -> Result<Eterm> {
    let arity = tuple_arity(t)?;
    if i >= arity {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `tuple_arity` established that `t` is a live tuple of `arity`
    // elements and `i < arity`, the bounds `tuple_element` requires.
    Ok(unsafe { beam_term::tuple_element(t, i) })
}

/// The head (`CAR`) of a cons cell; `NIL` or a non-list yields
/// [`BeamError::BadArg`].
pub fn cons_head(t: Eterm) -> Result<Eterm> {
    // `is_list` reads only the primary tag, never dereferences.
    if !beam_term::is_list(t) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_list` established that `t` is a header-less 2-word cons cell.
    Ok(unsafe { beam_term::car(t) })
}

/// The tail (`CDR`) of a cons cell; `NIL` or a non-list yields
/// [`BeamError::BadArg`].
pub fn cons_tail(t: Eterm) -> Result<Eterm> {
    // `is_list` reads only the primary tag, never dereferences.
    if !beam_term::is_list(t) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_list` established that `t` is a header-less 2-word cons cell.
    Ok(unsafe { beam_term::cdr(t) })
}

/// Collects the elements of a proper list into a `Vec`.
///
/// Walks the cons cells until [`NIL`](beam_term::NIL); the empty list yields an
/// empty `Vec`. A non-list term or an improper tail (the final `CDR` is neither a
/// cons cell nor `NIL`) yields [`BeamError::BadArg`].
pub fn list_elements(t: Eterm) -> Result<Vec<Eterm>> {
    let mut out = Vec::new();
    let mut cur = t;
    loop {
        if beam_term::is_nil(cur) {
            return Ok(out);
        }
        if !beam_term::is_list(cur) {
            // Improper tail, or a non-list to begin with.
            return Err(BeamError::BadArg);
        }
        // SAFETY: `is_list` established that `cur` is a live cons cell.
        unsafe {
            out.push(beam_term::car(cur));
            cur = beam_term::cdr(cur);
        }
    }
}

/// The `f64` value of a float term; a non-float yields [`BeamError::BadArg`].
///
/// [`beam_term::thing::float_value`] does not itself reject a non-float, so the
/// type check happens here.
pub fn float_value(t: Eterm) -> Result<f64> {
    // SAFETY: `is_float` checks `is_boxed` before dereferencing the header word.
    if !unsafe { beam_term::is_float(t) } {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_float` established that `t` is a live boxed float.
    Ok(unsafe { thing::float_value(t) })
}

/// Splits a map term into its `(key, value)` entries, in storage order.
///
/// Both frozen map layouts are read: a *flat* map (at most `MAP_SMALL_MAP_LIMIT`
/// pairs) through [`beam_term::thing::flatmap_parts`], and a **hashmap** (HAMT) by
/// walking its node tree ([`hashmap_entries`]). A term that is not a map, or a map
/// whose node words are not a well-shaped HAMT node, yields [`BeamError::BadArg`] —
/// never a wrong pair set.
///
/// The order is the map's *stored* order (flat-map insertion order; HAMT node
/// order). ERTS stores a flat map's keys in `erts_cmp_flatmap_keys` order, which
/// the module-local atom model cannot reproduce, so callers must treat the order as
/// implementation-defined and match keys with [`beam_term::cmp`].
pub fn map_entries(t: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    // SAFETY: `is_map`/`is_flatmap` check `is_boxed` before dereferencing.
    if !unsafe { beam_term::is_map(t) } {
        return Err(BeamError::BadArg);
    }
    if unsafe { beam_term::is_flatmap(t) } {
        return flatmap_entries(t);
    }
    // SAFETY: `is_map` established that `t` is a live boxed map; a non-flat map is
    // a HAMT whose root is a live head node.
    unsafe { hashmap_entries(t) }
}

/// Splits a *flat* map term into its `(key, value)` entries (stored order).
///
/// The `[head, size, keys, v0 … v(n-1)]` layout written by [`make_flatmap`]; the
/// root points at the `[head, size, keys]` prefix, so word 1 is the raw pair count.
fn flatmap_entries(t: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    // A well-formed flatmap never exceeds `MAP_SMALL_MAP_LIMIT` pairs.
    // SAFETY: `is_flatmap` established that `t` is a live flatmap, so word 1 is a
    // live, initialised term word.
    let n = unsafe { (*beam_term::boxed_val(t).add(1)).raw() };
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(BeamError::BadArg);
    }
    // SAFETY: the guards established that `t` is a live `n`-pair flat map.
    let (keys, values) = unsafe { thing::flatmap_parts(t) };
    Ok(keys.iter().copied().zip(values.iter().copied()).collect())
}

/// Walks a **hashmap** (HAMT) tree from its head node, collecting every leaf
/// `(key, value)` pair.
///
/// This is the enumeration of C's `hashmap_iterator_next`
/// (`erl_map.c:2346-2381`) and needs **no hash function**: the walk descends every
/// occupied slot of every node and leaves are the `[K | V]` cons cells, so key
/// matching stays with the caller's [`beam_term::cmp`] scan. (`erts_map_hash` is an
/// internal detail of how a *writer* chooses slots, not part of the frozen term
/// layout, and `beam_term::term_hash` is deliberately *not* it.)
///
/// The walk uses an explicit work stack rather than recursion, so a deeply nested
/// tree (at most `HAMT_MAX_LEVEL` = 16 levels, `erl_map.h:220`) cannot overflow the
/// native stack.
///
/// # Safety
///
/// `root` must be a live boxed HAMT head node (a live map term that is not a
/// flatmap).
unsafe fn hashmap_entries(root: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // SAFETY: `root` is a live head node (caller contract) and every value
        // pushed onto `stack` is a live sub-node read from a live node, so `node`
        // is a live boxed HAMT node.
        unsafe { collect_hashmap_node(node, &mut stack, &mut out)? };
    }
    Ok(out)
}

/// Reads one HAMT node: appends its leaf pairs and pushes its sub-nodes onto
/// `stack`.
///
/// # Safety
///
/// `node` must be a live boxed HAMT node of a well-formed tree — its header word
/// and every one of its item words must be live and in bounds.
unsafe fn collect_hashmap_node(
    node: Eterm,
    stack: &mut Vec<Eterm>,
    out: &mut Vec<(Eterm, Eterm)>,
) -> Result<()> {
    let base = beam_term::boxed_val(node);
    // SAFETY: `node` is a live boxed node, so its header word is live.
    let (first_item, count) = hashmap_node_shape(unsafe { *base })?;
    for i in 0..count {
        // SAFETY: a node of `count` items owns `first_item + count` words past its
        // header, so every item word is live and in bounds.
        let item = unsafe { *base.add(first_item + i) };
        // `THE_NON_VALUE` (the array-head "unoccupied slot" marker, `erl_map.h:75`)
        // is neither a leaf nor a sub-node, and `is_boxed` would *accept* it (its low
        // bit is clear), so it must be rejected before any dereference. ERTS fills
        // all 16 array-head slots before using an array head, but a hand-built or
        // corrupt term need not.
        if beam_term::is_non_value(item) {
            return Err(BeamError::BadArg);
        }
        if beam_term::is_list(item) {
            // A leaf: the `[K | V]` cons cell (`erl_map.c:2370-2372`).
            // SAFETY: `is_list` established that `item` is a live cons cell, so both
            // of its words are live.
            out.push(unsafe { (beam_term::car(item), beam_term::cdr(item)) });
        } else if beam_term::is_boxed(item) {
            // A sub-node: a bitmap, array or collision node.
            stack.push(item);
        } else {
            // An immediate in an item slot is not a HAMT node shape at all.
            return Err(BeamError::BadArg);
        }
    }
    Ok(())
}

/// The `(first-item word offset, item count)` of a HAMT node, from its header
/// word — the C `hashmap_node_size(hdr, &ptr)` together with its `ptr` bump
/// (`erl_map.c:2312-2335`).
///
/// The node layouts (`erl_map.h:142-205`):
/// * head array — `[MAP_HEADER_HAMT_HEAD_ARRAY, size, item × 16]`;
/// * head bitmap — `[header, size, item × popcount]` (`HAMT_HEAD_BITMAP_SZ(n) = 2 + n`);
/// * node bitmap — `[header, item × popcount]` (`HAMT_NODE_BITMAP_SZ(n) = 1 + n`);
/// * collision node — `[make_arityval(n), item × n]`.
fn hashmap_node_shape(hdr: Eterm) -> Result<(usize, usize)> {
    if beam_term::is_map_header(hdr) {
        let count = |hdr: Eterm| beam_term::map_header_val(hdr) as usize;
        match beam_term::map_header_type(hdr) as usize {
            beam_term::MAP_HEADER_TAG_HAMT_HEAD_ARRAY => {
                Ok((HAMT_HEAD_ITEM_OFFSET, HAMT_ARRAY_SLOTS))
            }
            beam_term::MAP_HEADER_TAG_HAMT_HEAD_BITMAP => {
                Ok((HAMT_HEAD_ITEM_OFFSET, count(hdr).count_ones() as usize))
            }
            beam_term::MAP_HEADER_TAG_HAMT_NODE_BITMAP => {
                Ok((HAMT_NODE_ITEM_OFFSET, count(hdr).count_ones() as usize))
            }
            // A flatmap head is read by [`flatmap_entries`], never as a HAMT node.
            _ => Err(BeamError::BadArg),
        }
    } else if beam_term::is_arity_value(hdr) {
        // A collision node: an untagged arityval header plus the leaves
        // (`MAP_HEADER_HAMT_COLLISION_NODE`, `erl_map.h:197`).
        Ok((HAMT_NODE_ITEM_OFFSET, beam_term::arityval(hdr)))
    } else {
        Err(BeamError::BadArg)
    }
}

/// The byte payload of an on-heap binary term; a non-binary (or a non-byte-aligned
/// bitstring) yields [`BeamError::BadArg`].
///
/// The returned slice borrows the term's storage, so the caller must keep the
/// owning heap alive and unmoved for as long as it is used.
pub fn binary_bytes<'a>(t: Eterm) -> Result<&'a [u8]> {
    // SAFETY: `is_binary` checks `is_boxed` before dereferencing the header word.
    if !unsafe { beam_term::is_binary(t) } {
        return Err(BeamError::BadArg);
    }
    // A binary larger than `ERL_ONHEAP_BINARY_LIMIT` bytes is stored **off** the
    // process heap: its term is the frozen 8-word `[BinRef][ErlSubBits]` pair, so the
    // heap path below would read the pair's own words instead of the payload.
    // `beam_term::refc::refc_bytes` re-derives the byte view from that frozen layout.
    // SAFETY: `t` is a live bitstring (checked above); if its header is an
    // `ErlSubBits` its `orig` slot is a live term, which is all `is_refc_binary`
    // dereferences.
    if unsafe { beam_term::refc::is_refc_binary(t) } {
        // SAFETY: `t` is a live refc-binary bitstring whose off-heap payload is owned
        // by the machine that built it (a `beam_alloc::RefcBinary` handle), so the
        // payload stays alive and unmoved for `'a` — the caller's contract.
        return Ok(unsafe { beam_term::refc::refc_bytes(t) });
    }
    // SAFETY: `is_binary` established that `t` is a live bitstring whose size is a
    // whole number of bytes; the size word (word 1) is live.
    let num_bytes = unsafe { beam_term::bitstring_size(t) } / 8;    if num_bytes == 0 {
        return Ok(&[]);
    }
    // SAFETY: a well-formed byte binary owns `2 + ceil(num_bytes / word_bytes)`
    // words, so the `num_bytes` data bytes starting at word 2 are live and in
    // bounds; the caller keeps the owning heap alive for `'a`.
    Ok(unsafe {
        core::slice::from_raw_parts(
            beam_term::boxed_val(t)
                .add(HEAP_BIN_HEAD_WORDS)
                .cast::<u8>(),
            num_bytes,
        )
    })
}

/// `is_any_fun(t)` — true for the boxed fun/closure term built by [`make_fun`].
pub fn is_fun(t: Eterm) -> bool {
    // SAFETY: `is_any_fun` checks `is_boxed` before dereferencing the header word.
    unsafe { beam_term::is_any_fun(t) }
}

/// The arity of a fun term; a non-fun yields [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_arity`] (the layout owner) over the term's
/// header word.
pub fn fun_arity(t: Eterm) -> Result<u32> {
    Ok(beam_term::fun::fun_arity(Eterm::from_raw(fun_header_word(t)?)) as u32)
}

/// The number of free variables (`env` size) of a fun term; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_num_free`] (the layout owner) over the term's
/// header word.
pub fn fun_num_free(t: Eterm) -> Result<usize> {
    Ok(beam_term::fun::fun_num_free(Eterm::from_raw(
        fun_header_word(t)?,
    )))
}

/// The unique identity (`uniq`) stored in a fun term; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_uniq`] (the layout owner), which reads the
/// [`FunKind::Local`](beam_term::fun::FunKind::Local) descriptor word — the only
/// kind this engine builds.
pub fn fun_uniq(t: Eterm) -> Result<u64> {
    // SAFETY: `t` must be a live fun term (the caller's contract; `make_fun` only
    // ever builds `FunKind::Local` funs); `fun_uniq` re-checks the kind before
    // reading the descriptor word.
    unsafe { beam_term::fun::fun_uniq(t) }.map_err(BeamError::from)
}

/// The lambda-table index stored in a fun term; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_index`] (the layout owner).
pub fn fun_index(t: Eterm) -> Result<u32> {
    // SAFETY: `t` must be a live fun term (the caller's contract); `fun_index`
    // re-checks the kind (`Local`/`Imported`) before reading the descriptor word.
    unsafe { beam_term::fun::fun_index(t) }.map_err(BeamError::from)
}

/// The free variables of a fun term, in `env` order; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_env`] (the layout owner). The returned slice
/// borrows the term's storage, so the caller must keep the owning heap alive and
/// unmoved.
pub fn fun_free<'a>(t: Eterm) -> Result<&'a [Eterm]> {
    // SAFETY: `t` must be a live fun term whose storage the caller keeps alive and
    // unmoved for `'a` (the documented borrow contract); `fun_env` re-checks the
    // fun subtag before computing the env slice.
    unsafe { beam_term::fun::fun_env(t) }.map_err(BeamError::from)
}

/// The raw header word of a fun term, or [`BeamError::BadArg`] if `t` is not a fun.
///
/// The type check [`fun_arity`]/[`fun_num_free`] share before forwarding the header
/// word to [`beam_term::fun`].
fn fun_header_word(t: Eterm) -> Result<usize> {
    if !is_fun(t) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_fun` established that `t` is boxed, so its header word is a live,
    // initialised term word.
    Ok(unsafe { (*beam_term::boxed_val(t)).raw() })
}

#[cfg(test)]
mod tests {
    //! Round-trips through the [`MinimalContext`] arena, plus raw-word layout
    //! assertions against stack buffers (allowed here — this is the SAFETY
    //! module) and the wrong-type / limit error cases.

    use super::*;
    use crate::context::MinimalContext;
    use beam_term::{make_small, NIL};

    fn ctx() -> MinimalContext {
        MinimalContext::new()
    }

    /// Asserts that a reader rejected a mismatched term with `BadArg`.
    fn assert_bad_arg<T: core::fmt::Debug>(r: Result<T>) {
        match r {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    // -- tuples ---------------------------------------------------------------

    #[test]
    fn tuple_round_trips_through_the_ctx() {
        let mut c = ctx();
        let (a, b, d) = (make_small(1), make_small(2), make_small(3));
        let t = make_tuple(&mut c, &[a, b, d]).unwrap();
        assert_eq!(tuple_arity(t).unwrap(), 3);
        assert_eq!(tuple_element(t, 0).unwrap(), a);
        assert_eq!(tuple_element(t, 1).unwrap(), b);
        assert_eq!(tuple_element(t, 2).unwrap(), d);
        assert_bad_arg(tuple_element(t, 3));
        assert_bad_arg(tuple_element(t, usize::MAX));
        assert_bad_arg(tuple_arity(a));
        assert_bad_arg(tuple_arity(NIL));
    }

    #[test]
    fn empty_tuple_is_the_shared_zero_arity_literal() {
        let mut c = ctx();
        let t = make_tuple(&mut c, &[]).unwrap();
        assert_eq!(t, beam_term::tuple0());
        assert_eq!(tuple_arity(t).unwrap(), 0);
    }

    #[test]
    fn tuple_layout_is_arityval_then_elements() {
        // Raw-word proof of the exact header/element offsets.
        let mut words = [make_small(0); 3];
        words[0] = beam_term::make_arityval(2);
        words[1] = make_small(10);
        words[2] = make_small(20);
        let t = beam_term::make_tuple(words.as_mut_ptr());
        assert_eq!(tuple_arity(t).unwrap(), 2);
        assert_eq!(tuple_element(t, 0).unwrap(), make_small(10));
        assert_eq!(tuple_element(t, 1).unwrap(), make_small(20));
        // SAFETY: `words` is the live 3-word tuple block backing `t`, so its header
        // word is a valid term word.
        assert_eq!(
            unsafe { *beam_term::boxed_val(t) },
            beam_term::make_arityval(2)
        );
    }

    // -- lists ----------------------------------------------------------------

    #[test]
    fn list_round_trips_through_the_ctx() {
        let mut c = ctx();
        let elems = [make_small(1), make_small(2), make_small(3)];
        let l = make_list(&mut c, &elems).unwrap();
        assert_eq!(list_elements(l).unwrap(), elems);
        assert_eq!(cons_head(l).unwrap(), elems[0]);
        let tail = cons_tail(l).unwrap();
        assert_eq!(list_elements(tail).unwrap(), elems[1..]);
    }

    #[test]
    fn empty_list_is_nil_and_nil_reads_as_empty() {
        let mut c = ctx();
        assert_eq!(make_list(&mut c, &[]).unwrap(), NIL);
        assert_eq!(list_elements(NIL).unwrap(), Vec::<Eterm>::new());
        assert_bad_arg(cons_head(NIL));
        assert_bad_arg(cons_tail(NIL));
    }

    #[test]
    fn improper_list_is_rejected_by_list_elements() {
        let cell = [make_small(1), make_small(2)];
        let improper = beam_term::make_list(cell.as_ptr());
        assert_eq!(cons_head(improper).unwrap(), make_small(1));
        assert_eq!(cons_tail(improper).unwrap(), make_small(2));
        assert_bad_arg(list_elements(improper));
        assert_bad_arg(cons_tail(make_small(3)));
    }

    #[test]
    fn cons_cell_layout_is_car_then_cdr() {
        let mut c = ctx();
        let l = make_list(&mut c, &[make_small(7)]).unwrap();
        // SAFETY: `l` is the live 2-word cons cell the context arena owns.
        unsafe {
            assert_eq!(*beam_term::list_val(l), make_small(7));
            assert_eq!(*beam_term::list_val(l).add(1), NIL);
        }
    }

    // -- cons (`put_list`) ----------------------------------------------------

    #[test]
    fn cons_round_trips_through_the_ctx() {
        let mut c = ctx();
        let (h, t) = (make_small(1), make_small(2));
        let cell = make_cons(&mut c, h, t).unwrap();
        assert!(beam_term::is_list(cell));
        assert_eq!(cons_head(cell).unwrap(), h);
        assert_eq!(cons_tail(cell).unwrap(), t);
        // SAFETY: `cell` is the live 2-word cons cell the arena owns.
        unsafe {
            assert_eq!(*beam_term::list_val(cell), h);
            assert_eq!(*beam_term::list_val(cell).add(1), t);
        }
    }

    #[test]
    fn cons_accepts_a_proper_nil_tail() {
        let mut c = ctx();
        let cell = make_cons(&mut c, make_small(1), NIL).unwrap();
        assert_eq!(cons_tail(cell).unwrap(), NIL);
        assert_eq!(list_elements(cell).unwrap(), vec![make_small(1)]);
    }

    #[test]
    fn cons_allows_an_improper_tail() {
        // `[1 | 2]` is a legal Erlang term; `list_elements` rejects it, but the
        // cell itself reads fine (this is what `put_list` must build).
        let mut c = ctx();
        let cell = make_cons(&mut c, make_small(1), make_small(2)).unwrap();
        assert_eq!(cons_head(cell).unwrap(), make_small(1));
        assert_eq!(cons_tail(cell).unwrap(), make_small(2));
        assert_bad_arg(list_elements(cell));
    }

    #[test]
    fn make_list_builds_from_cons_cells() {
        // `make_list` is now a fold over `make_cons`; the proper-list shape and
        // element order must be unchanged.
        let mut c = ctx();
        let elems = [make_small(10), make_small(20), make_small(30)];
        let l = make_list(&mut c, &elems).unwrap();
        let mut cursor = l;
        for elem in elems {
            assert_eq!(cons_head(cursor).unwrap(), elem);
            cursor = cons_tail(cursor).unwrap();
        }
        assert_eq!(cursor, NIL);
    }

    // -- badmatch reason ------------------------------------------------------

    #[test]
    fn badmatch_reason_is_a_tagged_tuple_when_the_atom_is_interned() {
        let mut c = ctx();
        let mut module = Module::default();
        let tag = module.atom_table.intern("badmatch");
        let value = make_small(42);
        let reason = badmatch_reason(&mut c, &module, value).unwrap();
        assert_eq!(tuple_arity(reason).unwrap(), 2);
        assert_eq!(tuple_element(reason, 0).unwrap(), make_atom(tag));
        assert_eq!(tuple_element(reason, 1).unwrap(), value);
    }

    #[test]
    fn badmatch_reason_is_a_typed_error_without_the_atom() {
        // `Module::default()` interns nothing, so `badmatch` cannot be
        // materialized: the canonical typed error, never a wrong atom.
        let mut c = ctx();
        let module = Module::default();
        assert!(matches!(
            badmatch_reason(&mut c, &module, make_small(1)),
            Err(BeamError::BadMatch)
        ));
    }

    // -- floats ---------------------------------------------------------------

    #[test]
    fn float_round_trips_through_the_ctx() {
        let mut c = ctx();
        let f = make_float(&mut c, 1.5).unwrap();
        assert_eq!(float_value(f).unwrap(), 1.5);
        // A negative zero survives the bit-exact write/read.
        let z = make_float(&mut c, -0.0).unwrap();
        assert!(float_value(z).unwrap().is_sign_negative());
    }

    #[test]
    fn float_value_rejects_a_non_float() {
        assert_bad_arg(float_value(make_small(1)));
        assert_bad_arg(float_value(NIL));
    }

    #[test]
    fn float_layout_is_header_then_bits() {
        let mut c = ctx();
        let f = make_float(&mut c, 2.0).unwrap();
        // SAFETY: `f` is the live 2-word float block the arena owns.
        unsafe {
            assert_eq!(*beam_term::boxed_val(f), beam_term::HEADER_FLONUM);
            assert_eq!(
                (*beam_term::boxed_val(f).add(1)).raw() as u64,
                2.0f64.to_bits()
            );
        }
    }

    // -- maps -----------------------------------------------------------------

    #[test]
    fn flatmap_round_trips_through_the_ctx() {
        let mut c = ctx();
        let pairs = [
            (make_small(1), make_small(10)),
            (make_small(2), make_small(20)),
        ];
        let m = make_flatmap(&mut c, &pairs).unwrap();
        assert_eq!(map_entries(m).unwrap(), pairs.to_vec());
    }

    #[test]
    fn empty_flatmap_round_trips() {
        let mut c = ctx();
        let m = make_flatmap(&mut c, &[]).unwrap();
        assert!(map_entries(m).unwrap().is_empty());
    }

    #[test]
    fn flatmap_over_the_small_limit_is_unsupported() {
        let mut c = ctx();
        let pairs: Vec<(Eterm, Eterm)> = (0..MAP_SMALL_MAP_LIMIT + 1)
            .map(|i| (make_small(i as i64), NIL))
            .collect();
        assert!(make_flatmap(&mut c, &pairs).is_err());
    }

    #[test]
    fn map_entries_rejects_a_non_map() {
        assert_bad_arg(map_entries(make_small(1)));
        assert_bad_arg(map_entries(NIL));
    }

    // -- hashmaps (HAMT read path) --------------------------------------------

    /// `MAP_HEADER_TAG_SZ` (`erl_map.h:181-183`) as a shift count.
    const MAP_HEADER_TAG_SZ: usize = beam_term::MAP_HEADER_TAG_SZ as usize;
    /// `MAP_HEADER_ARITY_SZ` (`erl_map.h:181-183`) as a shift count.
    const MAP_HEADER_ARITY_SZ: usize = beam_term::MAP_HEADER_ARITY_SZ as usize;

    /// `MAKE_MAP_HEADER(Type, Arity, Val)` (`erl_map.h:181-183`) — the frozen
    /// map-node header. Test-only: the engine *reads* a HAMT but never builds one
    /// (see [`map_entries`]).
    fn map_header(mtype: usize, arity: usize, val: usize) -> Eterm {
        let size = (((val << MAP_HEADER_ARITY_SZ) | arity) << MAP_HEADER_TAG_SZ) | mtype;
        beam_term::make_header(size, beam_term::MAP_SUBTAG)
    }

    /// A tiny in-test HAMT fixture.
    ///
    /// The rewrite has no HAMT *builder*, so these read-path tests hand-write the
    /// exact `erl_map.h` node blocks. Every block lives in a `Box<[Eterm]>` the
    /// fixture keeps alive, so a sub-node pointer stored in a parent node stays
    /// valid (moving the `Box` moves the pointer, never the pointed-to block).
    struct Hamt {
        blocks: Vec<Box<[Eterm]>>,
    }

    impl Hamt {
        fn new() -> Hamt {
            Hamt { blocks: Vec::new() }
        }

        /// Store `words` and tag the block as a **boxed** term (a map node).
        fn node(&mut self, words: Vec<Eterm>) -> Eterm {
            let block = words.into_boxed_slice();
            let term = beam_term::make_boxed(block.as_ptr());
            self.blocks.push(block);
            term
        }

        /// Store `[K, V]` and tag the block as a **list** term (a leaf cell).
        fn leaf(&mut self, key: Eterm, value: Eterm) -> Eterm {
            let cell = vec![key, value].into_boxed_slice();
            let term = beam_term::make_list(cell.as_ptr());
            self.blocks.push(cell);
            term
        }

        /// `[MAP_HEADER_HAMT_HEAD_BITMAP(bitmap), size, item…]` — a bitmap head.
        fn head_bitmap(&mut self, bitmap: u16, size: usize, items: &[Eterm]) -> Eterm {
            assert_eq!(bitmap.count_ones() as usize, items.len());
            let mut words = vec![
                map_header(
                    beam_term::MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
                    1,
                    usize::from(bitmap),
                ),
                Eterm::from_raw(size),
            ];
            words.extend_from_slice(items);
            self.node(words)
        }

        /// `[MAP_HEADER_HAMT_HEAD_ARRAY, size, item × 16]` — an array head.
        fn head_array(&mut self, size: usize, items: &[Eterm]) -> Eterm {
            assert_eq!(items.len(), HAMT_ARRAY_SLOTS);
            let mut words = vec![
                map_header(beam_term::MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff),
                Eterm::from_raw(size),
            ];
            words.extend_from_slice(items);
            self.node(words)
        }

        /// `[MAP_HEADER_HAMT_NODE_BITMAP(bitmap), item…]` — an interior node.
        fn node_bitmap(&mut self, bitmap: u16, items: &[Eterm]) -> Eterm {
            assert_eq!(bitmap.count_ones() as usize, items.len());
            let mut words = vec![map_header(
                beam_term::MAP_HEADER_TAG_HAMT_NODE_BITMAP,
                0,
                usize::from(bitmap),
            )];
            words.extend_from_slice(items);
            self.node(words)
        }

        /// `[make_arityval(n), item…]` — a collision node.
        fn collision(&mut self, items: &[Eterm]) -> Eterm {
            let mut words = vec![beam_term::make_arityval(items.len())];
            words.extend_from_slice(items);
            self.node(words)
        }
    }

    #[test]
    fn a_hand_built_head_node_is_recognised_as_a_hashmap() {
        let mut hamt = Hamt::new();
        let leaf = hamt.leaf(make_small(1), make_small(10));
        let root = hamt.head_bitmap(0b0001, 1, &[leaf]);
        // SAFETY: `root` is the live head node the fixture owns.
        unsafe {
            assert!(beam_term::is_map(root));
            assert!(beam_term::is_hashmap(root));
            assert!(!beam_term::is_flatmap(root));
        }
    }

    #[test]
    fn map_entries_reads_a_bitmap_head() {
        let (k1, k2) = (make_small(1), make_small(2));
        let mut hamt = Hamt::new();
        let first = hamt.leaf(k1, make_small(10));
        let second = hamt.leaf(k2, make_small(20));
        let root = hamt.head_bitmap(0b0011, 2, &[first, second]);
        assert_eq!(
            map_entries(root).unwrap(),
            vec![(k1, make_small(10)), (k2, make_small(20))]
        );
    }

    #[test]
    fn map_entries_descends_into_sub_nodes() {
        // Head slot 0 → a leaf; head slot 1 → a bitmap node holding two leaves.
        let mut hamt = Hamt::new();
        let top = hamt.leaf(make_small(1), make_small(10));
        let inner_first = hamt.leaf(make_small(2), make_small(20));
        let inner_second = hamt.leaf(make_small(3), make_small(30));
        let inner = hamt.node_bitmap(0b0110, &[inner_first, inner_second]);
        let root = hamt.head_bitmap(0b0011, 3, &[top, inner]);
        assert_eq!(
            map_entries(root).unwrap(),
            vec![
                (make_small(1), make_small(10)),
                (make_small(2), make_small(20)),
                (make_small(3), make_small(30)),
            ]
        );
    }

    #[test]
    fn map_entries_reads_an_array_head() {
        let mut hamt = Hamt::new();
        let mut items = Vec::new();
        for i in 0..HAMT_ARRAY_SLOTS as i64 {
            items.push(hamt.leaf(make_small(i), make_small(i + 100)));
        }
        let root = hamt.head_array(HAMT_ARRAY_SLOTS, &items);
        let entries = map_entries(root).unwrap();
        assert_eq!(entries.len(), HAMT_ARRAY_SLOTS);
        for (i, (key, value)) in entries.iter().enumerate() {
            assert_eq!(*key, make_small(i as i64));
            assert_eq!(*value, make_small(i as i64 + 100));
        }
    }

    #[test]
    fn map_entries_reads_a_collision_node() {
        let mut hamt = Hamt::new();
        let first = hamt.leaf(make_small(1), make_small(10));
        let second = hamt.leaf(make_small(2), make_small(20));
        let child = hamt.collision(&[first, second]);
        // A bitmap head whose only occupied slot holds the collision node.
        let root = hamt.head_bitmap(0b0001, 2, &[child]);
        assert_eq!(
            map_entries(root).unwrap(),
            vec![(make_small(1), make_small(10)), (make_small(2), make_small(20))]
        );
    }

    #[test]
    fn map_entries_rejects_malformed_hamt_nodes() {
        // An array head with `THE_NON_VALUE` in an unoccupied slot: a typed error,
        // never a bogus pair (and never a dereference of the non-value, which
        // `is_boxed` would otherwise accept).
        let mut hamt = Hamt::new();
        let mut items = vec![beam_term::THE_NON_VALUE; HAMT_ARRAY_SLOTS];
        items[0] = hamt.leaf(make_small(1), make_small(10));
        let root = hamt.head_array(1, &items);
        assert_bad_arg(map_entries(root));

        // An item slot holding an immediate is not a HAMT node shape.
        let mut hamt = Hamt::new();
        let root = hamt.head_bitmap(0b0001, 1, &[make_small(7)]);
        assert_bad_arg(map_entries(root));

        // A boxed term whose header is not a map node at all.
        let mut words = [make_small(0)];
        let not_a_node = beam_term::make_boxed(words.as_mut_ptr());
        assert_bad_arg(map_entries(not_a_node));
    }
    // -- funs -----------------------------------------------------------------

    #[test]
    fn fun_round_trips_through_the_ctx() {
        let mut c = ctx();
        let free = [make_small(11), make_small(22)];
        let f = make_fun(&mut c, 3, 0xDEAD_BEEF_CAFE, 7, &free).unwrap();
        assert!(is_fun(f));
        assert_eq!(fun_arity(f).unwrap(), 3);
        assert_eq!(fun_num_free(f).unwrap(), 2);
        assert_eq!(fun_uniq(f).unwrap(), 0xDEAD_BEEF_CAFE);
        assert_eq!(fun_index(f).unwrap(), 7);
        assert_eq!(fun_free(f).unwrap(), &free[..]);
    }

    #[test]
    fn fun_without_free_variables_reads_an_empty_env() {
        let mut c = ctx();
        let f = make_fun(&mut c, 0, 1, 2, &[]).unwrap();
        assert!(is_fun(f));
        assert_eq!(fun_arity(f).unwrap(), 0);
        assert_eq!(fun_num_free(f).unwrap(), 0);
        assert_eq!(fun_free(f).unwrap(), &[] as &[Eterm]);
    }

    #[test]
    fn fun_layout_is_header_uniq_index_env() {
        // Raw-word proof of the exact block the widened interpreter relies on.
        let mut words = [Eterm::default(); 3 + 1];
        words[0] = beam_term::fun::make_fun_header(2, 1, beam_term::fun::FunKind::Local).unwrap();
        words[1] = Eterm::from_raw(0x1234_5678_9ABC);
        words[2] = Eterm::from_raw(9);
        words[3] = make_small(42);
        let f = beam_term::make_boxed(words.as_ptr());
        assert!(is_fun(f));
        assert_eq!(fun_arity(f).unwrap(), 2);
        assert_eq!(fun_num_free(f).unwrap(), 1);
        assert_eq!(fun_uniq(f).unwrap(), 0x1234_5678_9ABC);
        assert_eq!(fun_index(f).unwrap(), 9);
        assert_eq!(fun_free(f).unwrap(), &[make_small(42)]);
    }

    #[test]
    fn fun_readers_reject_non_funs() {
        assert!(!is_fun(make_small(1)));
        assert!(!is_fun(NIL));
        assert_bad_arg(fun_arity(make_small(1)));
        assert_bad_arg(fun_num_free(NIL));
        assert_bad_arg(fun_uniq(NIL));
        assert_bad_arg(fun_index(NIL));
        assert_bad_arg(fun_free(NIL));
        // A tuple is boxed but not a fun.
        let mut c = ctx();
        let t = make_tuple(&mut c, &[make_small(1)]).unwrap();
        assert!(!is_fun(t));
        assert_bad_arg(fun_arity(t));
    }

    #[test]
    fn fun_fields_over_255_are_a_system_limit() {
        let mut c = ctx();
        assert!(make_fun(&mut c, 256, 1, 1, &[]).is_err());
        let free = vec![make_small(0); 256];
        assert!(make_fun(&mut c, 1, 1, 1, &free).is_err());
    }

    // -- integers ----------------------------------------------------------

    #[test]
    fn make_integer_keeps_small_values_small() {
        let mut c = ctx();
        for v in [0i64, 1, -1, MAX_SMALL, MAX_SMALL - 1, MIN_SMALL, MIN_SMALL + 1] {
            let t = make_integer(&mut c, v).unwrap();
            assert!(beam_term::is_small(t), "{v} must stay a small int");
            assert_eq!(beam_term::signed_value(t), v);
        }
    }

    #[test]
    fn make_integer_builds_an_exact_bignum_outside_the_small_range() {
        let mut c = ctx();
        let cases = [
            (MAX_SMALL + 1, false),
            (MIN_SMALL - 1, true),
            (999_999_999_999_999_999, false),
            (-999_999_999_999_999_999, true),
            (i64::MAX, false),
            (i64::MIN, true),
        ];
        for (v, negative) in cases {
            let t = make_integer(&mut c, v).unwrap();
            assert!(!beam_term::is_small(t), "{v} must not be a small int");
            // SAFETY: `t` is a live bignum just written by `make_integer`.
            let (neg, digits) = unsafe { beam_term::big_parts(t) };
            assert_eq!(neg, negative, "{v}");
            assert_eq!(digits, [v.unsigned_abs()], "{v}");
        }
    }

    // -- binaries -------------------------------------------------------------

    #[test]
    fn heap_binary_round_trips_through_the_ctx() {
        let mut c = ctx();
        let bytes = b"hello, beam";
        let b = make_heap_binary(&mut c, bytes).unwrap();
        assert_eq!(binary_bytes(b).unwrap(), &bytes[..]);
    }

    #[test]
    fn empty_heap_binary_round_trips() {
        let mut c = ctx();
        let b = make_heap_binary(&mut c, &[]).unwrap();
        assert_eq!(binary_bytes(b).unwrap(), &[] as &[u8]);
    }

    #[test]
    fn heap_binary_layout_is_header_size_then_data() {
        let mut c = ctx();
        let b = make_heap_binary(&mut c, b"abc").unwrap();
        // SAFETY: `b` is the live heap-binary block the arena owns.
        unsafe {
            let p = beam_term::boxed_val(b);
            // header: block words (2 + 1) minus the header word, heap-bits subtag.
            assert_eq!(
                *p,
                beam_term::make_header(2 + 1 - 1, beam_term::HEAP_BITS_SUBTAG)
            );
            // size is the *bit* count.
            assert_eq!((*p.add(1)).raw(), 3 * 8);
        }
        assert_eq!(binary_bytes(b).unwrap(), &b"abc"[..]);
    }

    #[test]
    fn heap_binary_over_the_limit_is_unsupported() {
        let mut c = ctx();
        let bytes = vec![0u8; ERL_ONHEAP_BINARY_LIMIT + 1];
        assert!(make_heap_binary(&mut c, &bytes).is_err());
    }

    #[test]
    fn binary_bytes_rejects_non_binaries() {
        assert_bad_arg(binary_bytes(make_small(1)));
        assert_bad_arg(binary_bytes(NIL));
    }
}
