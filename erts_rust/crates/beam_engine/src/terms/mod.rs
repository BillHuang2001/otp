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

//! Boxed-term construction and reading behind the execution-context heap seam —
//! the engine's **second** `unsafe` module.
//!
//! The interpreter has to *build* boxed terms (`put_tuple2`, `put_list`, float
//! literals, `make_fun`, binary construction, …) and *read* them back
//! (`get_tuple_element`, `get_list`, map access, fun introspection). Term
//! construction/inspection itself is owned by `beam_term` (the frozen-layout
//! lowest common ancestor, workspace `CONTEXT.md` §2), but `beam_term` is a
//! dependency *sink*: it never allocates. This module is the glue that obtains raw
//! storage from [`ExecutionContext::alloc_words`](crate::context::ExecutionContext::alloc_words)
//! and passes it to the `beam_term` writers — the fun header/descriptor layout is
//! owned by [`beam_term::fun`] and only the **heap binary** layout is hand-rolled
//! here.
//!
//! # Ownership model
//!
//! * The **context owns the heap**.
//!   [`ExecutionContext::alloc_words`](crate::context::ExecutionContext::alloc_words)
//!   hands out a stable pointer to `n` uninitialized words; those words stay valid
//!   and unmoved until the next
//!   [`ExecutionContext::ensure_heap`](crate::context::ExecutionContext::ensure_heap)
//!   call (a GC), exactly as the C `HAlloc`/`heap_top` convention. Nothing here
//!   frees or moves them.
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
//! The **hashmap (HAMT) read *and* build paths** are hand-rolled here as well:
//! `beam_term` owns the frozen map-header tag constants (`HAMT_SUBTAG_*`), the
//! `is_hashmap` classifier and the HAMT builder/reader
//! ([`beam_term::map::write_map`]), so [`map_entries`] walks the `erl_map.h` node
//! tree itself while [`make_map`] delegates construction to
//! [`beam_term::map::write_map`] — a flat map at/below `MAP_SMALL_MAP_LIMIT` (32)
//! pairs, a hashmap above it, exactly the oracle's choice.
//!
//! # Error strategy
//!
//! Everything returns [`beam_common::Result`] (never a crate-local enum): a wrong
//! term type or an out-of-range index is
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg), an unimplemented
//! case is [`crate::error::unsupported`], and a hard representation limit is
//! [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit). No function
//! panics on runtime data.
//!
//! # Module layout
//!
//! It is the crate's single raw-heap boxed-term builder/reader (the widening
//! contract: one `#[allow(unsafe_code)]` home for every term layout), split by
//! responsibility into submodules under that one home — `construct` (the
//! heap-seam builders), `read` (the tuple/list/binary readers), `map` (the
//! flat-map/HAMT builder and reader), `fun` (the fun builder and readers) and
//! `number` (the integer/float helpers) — plus a comprehensive in-module test
//! suite (raw-word layout assertions and the HAMT fixtures). Every submodule is
//! re-exported here, so the public paths (`crate::terms::make_tuple`,
//! `crate::terms::map_entries`, …) are unchanged.

use beam_term::Eterm;

mod construct;
mod fun;
mod map;
mod number;
mod read;
mod sink;
mod slack;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
pub(crate) mod alloc_tests;

pub use construct::{badmatch_reason, make_cons, make_heap_binary, make_list, make_tuple};
pub use fun::{fun_arity, fun_free, fun_index, fun_num_free, fun_uniq, is_fun, make_fun};
pub use map::{is_hashmap, make_flatmap, make_map, map_entries};
pub use number::{float_value, make_float, make_integer};
pub(crate) use number::make_integer_unrooted;
pub use read::{binary_bytes, cons_head, cons_tail, list_elements, tuple_arity, tuple_element};
pub use sink::{ContextSink, Sink};
pub(crate) use sink::{write_list_at, write_tuple_at};
pub use slack::Slack;
pub(crate) use slack::reserve;
pub use slack::reserve_slack;

/// `MAP_SMALL_MAP_LIMIT` (`erl_map.h:93-97`): the largest flat-map pair count.
/// Above it ERTS uses a **hashmap** (HAMT), which [`make_map`] builds; this
/// constant is the flat-layout boundary [`make_flatmap`] enforces and
/// `flatmap_entries` validates against.
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

#[cfg(test)]
mod tests;
