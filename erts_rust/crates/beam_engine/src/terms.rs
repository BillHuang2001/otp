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
//! `beam_term` writers (or, where `beam_term` owns no layout — funs, and the heap
//! binary — writes the frozen C layout directly).
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
//! Reuse is deliberate: tuples, cons cells, floats, flat maps, bignums and the
//! external pid/port/ref blocks are written by the matching `beam_term` primitive;
//! only the two families `beam_term` does not own — **funs** and the **heap
//! binary** — hand-roll a layout here, each pinned line-by-line to the C ground
//! truth in the function docs.
//!
//! # Error strategy
//!
//! Everything returns [`beam_common::Result`] (never a crate-local enum): a wrong
//! term type or an out-of-range index is [`BeamError::BadArg`], an unimplemented
//! case is [`crate::error::unsupported`], and a hard representation limit is
//! [`BeamError::SystemLimit`]. No function panics on runtime data.

use beam_common::{BeamError, Result};
use beam_loader::Module;
use beam_term::{make_atom, thing, Eterm};

use crate::context::ExecutionContext;

/// `FUN_HEADER_ARITY_OFFS` (`erl_term.h:413`): `_HEADER_ARITY_OFFS + 2`.
const FUN_HEADER_ARITY_OFFS: u32 = 8;
/// `FUN_HEADER_ENV_SIZE_OFFS` (`erl_term.h:414`): `FUN_HEADER_ARITY_OFFS + 8`.
const FUN_HEADER_ENV_SIZE_OFFS: u32 = 16;
/// The arity / environment size fields of a fun header are 8 bits
/// (`erl_term.h:404-410`: "both limited to 255").
const FUN_FIELD_MAX: usize = 0xFF;
/// The M1 fun-entry descriptor words inlined after the header (`uniq`, `index`) —
/// see [`make_fun`] for why the C `entry` pointer is expanded.
const FUN_DESCRIPTOR_WORDS: usize = 2;
/// Word offset of the first free variable (`env[0]`): the header plus the inlined
/// descriptor.
const FUN_ENV_OFFSET: usize = 1 + FUN_DESCRIPTOR_WORDS;

/// `MAP_SMALL_MAP_LIMIT`: the largest flat-map pair count. Above it ERTS uses a
/// hashmap, which this module does not build (that is `beam_codec`'s HAMT
/// builder); [`make_flatmap`] rejects it.
const MAP_SMALL_MAP_LIMIT: usize = 32;

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

/// Builds a small *flat* map term from `pairs` (insertion order preserved).
///
/// The `[arityval(n), k0 … k(n-1), head, n, keys, v0 … v(n-1)]` layout is written
/// by [`beam_term::thing::write_flatmap`]. Duplicate keys are not detected here
/// (that needs term comparison). More than `MAP_SMALL_MAP_LIMIT` (32) pairs needs
/// the HAMT builder and yields [`crate::error::unsupported`].
pub fn make_flatmap(ctx: &mut dyn ExecutionContext, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let n = pairs.len();
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(crate::error::unsupported("flatmap > 32 pairs"));
    }
    let hp = ctx.alloc_words(thing::flatmap_size_words(n))?;
    // SAFETY: `alloc_words` returned `flatmap_size_words(n)` writable words and
    // `n <= MAP_SMALL_MAP_LIMIT`, the preconditions `write_flatmap` documents.
    Ok(unsafe { thing::write_flatmap(hp, pairs) })
}

/// Builds a local **fun** (closure) term.
///
/// # Frozen word layout (M1)
///
/// C's `ErlFunThing` (`erl_fun.h:65-99`) is
/// `[thing_word, entry, env[0..num_free]]`, i.e. `ERL_FUN_SIZE (= 2) + num_free`
/// words, where `thing_word = MAKE_FUN_HEADER(Arity, NumFree, 0)`
/// (`erl_term.h:413-424`; the header packs arity at bit 8, the environment size at
/// bit 16 and the external-fun kind at bit 24 — 8 bits each, hence the 255 limit),
/// and `entry` is a **pointer** to an out-of-line `ErlFunEntry`/`Export` that owns
/// the code address, `uniq[16]` and `index` (`erl_fun.c`, `emu/instrs.tab:545-584`
/// `make_fun3`).
///
/// M1 has no code addresses or fun-entry table, so the single `entry` pointer is
/// **inlined as two descriptor words** — this is the same documented
/// "M1 substitution" idea as the external-pid `node` slot (see
/// `beam_term::thing::write_external_pid`). The resulting block is
/// `1 (header) + 2 (uniq, index) + num_free` words:
///
/// ```text
/// word 0            MAKE_FUN_HEADER(arity, num_free, external = 0)  (exact C encoding)
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
    if arity as usize > FUN_FIELD_MAX || num_free > FUN_FIELD_MAX {
        return Err(BeamError::SystemLimit);
    }
    let hp = ctx.alloc_words(1 + FUN_DESCRIPTOR_WORDS + num_free)?;
    // SAFETY: `alloc_words` returned `1 + 2 + num_free` exclusively-owned, writable
    // words. Word 0 receives the fun header, word 1 the `uniq`, word 2 the `index`
    // and words `3..3+num_free` the free variables — every write is in bounds.
    let root = unsafe {
        *hp = fun_header(arity as usize, num_free);
        *hp.add(1) = Eterm::from_raw(uniq as usize);
        *hp.add(2) = Eterm::from_raw(index as usize);
        for (i, var) in free.iter().enumerate() {
            *hp.add(FUN_ENV_OFFSET + i) = *var;
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

/// `MAKE_FUN_HEADER(arity, num_free, external = 0)` (`erl_term.h:417-424`).
///
/// `_TAG_HEADER_FUN` is `TAG_PRIMARY_HEADER | FUN_SUBTAG`; the arity starts at bit
/// `FUN_HEADER_ARITY_OFFS` (8) and the environment size at bit
/// `FUN_HEADER_ENV_SIZE_OFFS` (16). The external-fun kind bit (24) is left clear
/// — this module builds only local funs. The caller has already bounded both
/// fields to 8 bits.
fn fun_header(arity: usize, num_free: usize) -> Eterm {
    Eterm::from_raw(
        beam_term::FUN_SUBTAG
            | (arity << FUN_HEADER_ARITY_OFFS)
            | (num_free << FUN_HEADER_ENV_SIZE_OFFS),
    )
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

/// Splits a *flat* map term into its `(key, value)` entries (wire order).
///
/// A term that is not a map yields [`BeamError::BadArg`]; a hashmap (a map beyond
/// the flat-map limit) yields [`crate::error::unsupported`] — this module only
/// reads the [`make_flatmap`] layout.
pub fn map_entries(t: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    // SAFETY: `is_map`/`is_flatmap` check `is_boxed` before dereferencing.
    if !unsafe { beam_term::is_map(t) } {
        return Err(BeamError::BadArg);
    }
    if !unsafe { beam_term::is_flatmap(t) } {
        return Err(crate::error::unsupported("map_entries: hashmap"));
    }
    // A well-formed flatmap never exceeds `MAP_SMALL_MAP_LIMIT` pairs; the root
    // points at the `[head, size, keys]` prefix, so word 1 is the raw pair count.
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
    // SAFETY: `is_binary` established that `t` is a live bitstring whose size is a
    // whole number of bytes; the size word (word 1) is live.
    let num_bytes = unsafe { beam_term::bitstring_size(t) } / 8;
    if num_bytes == 0 {
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
pub fn fun_arity(t: Eterm) -> Result<u32> {
    Ok(((fun_header_word(t)? >> FUN_HEADER_ARITY_OFFS) & FUN_FIELD_MAX as usize) as u32)
}

/// The number of free variables (`env` size) of a fun term; a non-fun yields
/// [`BeamError::BadArg`].
pub fn fun_num_free(t: Eterm) -> Result<usize> {
    Ok((fun_header_word(t)? >> FUN_HEADER_ENV_SIZE_OFFS) & FUN_FIELD_MAX as usize)
}

/// The unique identity (`uniq`) stored in a fun term; a non-fun yields
/// [`BeamError::BadArg`].
pub fn fun_uniq(t: Eterm) -> Result<u64> {
    fun_header_word(t)?;
    // SAFETY: `fun_header_word` established that `t` is a live fun, so descriptor
    // word 1 is live and initialised.
    Ok(unsafe { (*beam_term::boxed_val(t).add(1)).raw() as u64 })
}

/// The lambda-table index stored in a fun term; a non-fun yields
/// [`BeamError::BadArg`].
pub fn fun_index(t: Eterm) -> Result<u32> {
    fun_header_word(t)?;
    // SAFETY: as [`fun_uniq`] — descriptor word 2 is live and initialised.
    Ok(unsafe { (*beam_term::boxed_val(t).add(2)).raw() as u32 })
}

/// The free variables of a fun term, in `env` order; a non-fun yields
/// [`BeamError::BadArg`].
///
/// The returned slice borrows the term's storage (see [`make_fun`] for the
/// layout), so the caller must keep the owning heap alive and unmoved.
pub fn fun_free<'a>(t: Eterm) -> Result<&'a [Eterm]> {
    let num_free = fun_num_free(t)?;
    if num_free == 0 {
        return Ok(&[]);
    }
    // SAFETY: `fun_num_free` established that `t` is a live fun of `num_free` free
    // variables, which start at word `FUN_ENV_OFFSET`; the block is that plus
    // `num_free` words, so the slice is live and in bounds. The caller keeps the
    // owning heap alive for `'a`.
    Ok(unsafe {
        core::slice::from_raw_parts(beam_term::boxed_val(t).add(FUN_ENV_OFFSET), num_free)
    })
}

/// The raw header word of a fun term, or [`BeamError::BadArg`] if `t` is not a fun.
///
/// Shared by the fun readers so the type check lives in exactly one place.
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
    fn map_entries_rejects_a_non_map_and_a_hashmap() {
        assert_bad_arg(map_entries(make_small(1)));
        assert_bad_arg(map_entries(NIL));
        // A HAMT head header is a map but not a *flat* map.
        let mut words = [Eterm::from_raw(beam_term::HAMT_SUBTAG_HEAD_ARRAY)];
        let hm = beam_term::make_boxed(words.as_mut_ptr());
        assert!(map_entries(hm).is_err());
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
        words[0] = fun_header(2, 1);
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
