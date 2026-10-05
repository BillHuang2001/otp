# `beam_term` — Frozen BEAM Term Representation

## Intent
Owns the **frozen, bit-exact tagged term representation** of the Rust ERTS rewrite: the single most
load-bearing shared contract in the workspace (workspace `CONTEXT.md` §2). Implements Part I §A.1–A.2
(language semantics + `.beam` data model) and the term-layout discussion of Part II §B, at **Level-1
drop-in fidelity**: a Rust core and the retained C periphery must not differ by one bit.

Ground truth = the legacy C (`../../erts/emulator/beam/erl_term.h`, `erl_term.c`); this crate reproduces
that layout exactly. Every other crate depends downward on this one for term construction, inspection
and classification. This node is the lowest common ancestor for term operations: **no other crate may
assume a different layout.**

## API Surface (FROZEN — every downstream crate routes here)

`Eterm` is a `#[repr(transparent)]` newtype over `usize`, ABI-identical to a NIF's `ERL_NIF_TERM`
(both are one machine word). `beam_abi` will transmute between them; no other crate may redefine it.

### crate root (`src/lib.rs`)
`pub mod {tag, eterm, immediate, atom, list, boxed, tuple, heap, literal, ops, compare, hashing, size, thing, bitstring, refc, map, error}` plus
`pub mod prelude` and flat `pub use` re-exports of every public item below, so downstream code can
`use beam_term::prelude::*;`.

### `eterm.rs` — the word type
- `pub struct Eterm(usize)` — `#[repr(transparent)]`; `Copy + Clone + PartialEq + Eq + Hash + Debug`.
- `pub const fn Eterm::raw(self) -> usize`
- `pub const fn Eterm::from_raw(raw: usize) -> Eterm`
- `impl Default for Eterm` → `NIL` (matches BEAM's "blank = NIL" init convention).
- `pub const fn Eterm::nil() -> Eterm`; `pub const fn Eterm::non_value() -> Eterm`.

### `tag.rs` — raw tag constants, masks, shifts, enums
- Primary: `TAG_PRIMARY_MASK=0x3`, `TAG_PRIMARY_HEADER=0x0`, `TAG_PRIMARY_LIST=0x1`,
  `TAG_PRIMARY_BOXED=0x2`, `TAG_PRIMARY_IMMED1=0x3`.
- Pointer: `TAG_PTR_MASK=0x7` (64-bit), `TAG_LITERAL_PTR=0x4`.
- IMMED1 (mask `0xF`): `TAG_IMMED1_SIZE=4`, `TAG_IMMED1_PID=0x3`, `TAG_IMMED1_PORT=0x7`,
  `TAG_IMMED1_IMMED2=0xB`, `TAG_IMMED1_SMALL=0xF`.
- IMMED2 (mask `0x3F`): `TAG_IMMED2_SIZE=6`, `TAG_IMMED2_ATOM=0xB`, `TAG_IMMED2_CATCH=0x1B`,
  `TAG_IMMED2_NIL=0x3B`.
- Headers: `TAG_HEADER_MASK=0x3F`, `HEADER_ARITY_OFFS=6`, `HEADER_SUBTAG_MASK=0x3C`,
  `MAX_ARITYVAL=(1<<24)-1`.
- `pub enum PrimaryTag { Header, List, Boxed, Immed1 }`
- `pub enum BoxedTag { ArityVal=0x00, PosBig=0x08, NegBig=0x0C, Ref=0x10, Fun=0x14, Float=0x18,
  Record=0x1C, HeapBits=0x20, SubBits=0x24, BinRef=0x28, Map=0x2C, ExternalPid=0x30,
  ExternalPort=0x34, ExternalRef=0x38 }` (+ `BoxedTag::from_raw(subtag)`).
- `pub const fn primary_tag(Eterm) -> PrimaryTag`; `pub const fn is_header(Eterm) -> bool`.

### `immediate.rs` — immediates
- Small ints: `SMALL_BITS=60`, `MAX_SMALL=(1<<59)-1`, `MIN_SMALL=-(1<<59)`;
  `is_immediate`/`is_not_immediate`; `is_small`/`is_not_small`; `make_small(i64) -> Eterm` (unchecked,
  `debug_assert` in range); `try_make_small(i64) -> TermResult<Eterm>`; `unsigned_value(Eterm) -> u64`;
  `signed_value(Eterm) -> i64`; `is_byte(Eterm) -> bool`.
- Atoms (immediate form): `is_atom`/`is_not_atom`; `make_atom(Atom) -> Eterm`; `atom_val(Eterm) -> Atom`.
- NIL: `pub const NIL: Eterm` (= `0x3B`); `is_nil`/`is_not_nil`.
- Catch: `make_catch(u64)`, `is_catch`, `catch_val(Eterm) -> u64`.
- Pid/port immediates: `is_internal_pid`, `make_pid_data(ser,num)`, `pid_number`, `pid_serial`;
  `is_internal_port`, `make_port_data(num)`, `port_number`.

### `atom.rs` — atom indices + interning table
- `pub struct Atom(u32)` — `Copy + Clone + PartialEq + Eq + Hash + PartialOrd + Ord + Debug`.
- `pub const ATOM_INDEX_BITS: u32 = 6`; `pub const MAX_ATOM_INDEX: usize = 2^58-1` (representation
  ceiling); `Atom::MAX` (= `u32::MAX`, the practical ceiling).
- `pub struct AtomTable` — deterministic: **indices are assigned in insertion order starting at 0**,
  reproducing the `atom.names` (`../../erts/emulator/beam/atom.names`) ordering so preloaded modules
  resolve correctly. One index per distinct name.
- `AtomTable::new() -> AtomTable`; `AtomTable::intern(&mut self, name: &str) -> Atom`;
  `lookup(&self, name: &str) -> Option<Atom>`; `name(&self, Atom) -> Option<&str>`; `len`, `is_empty`.
- `AtomTable::seed(&mut self, ordered_names: impl IntoIterator<Item = &str>)` — pre-intern in order.
- `AtomTable::seed_from_atom_names(&mut self, src: &str) -> TermResult<()>` — parse the `atom.names`
  grammar (`atom` <name>+ ; names are bare, `'quoted'`, or `CName='printname'`; `#` comments) and
  intern in file order. `from_atom_names(src) -> TermResult<AtomTable>` convenience.
- Note: generating the standard `am_*`-equivalent `Atom` constants from the full `atom.names` is a
  codegen/boot step (routed to `beam_rt`); this crate owns only the mechanism + parser.

### `list.rs` — cons cells (`[CAR,CDR]`, header-less, `TAG_PRIMARY_LIST`)
- `make_list(*const Eterm) -> Eterm`; `list_val(Eterm) -> *mut Eterm`; `ptr_val(Eterm) -> *mut Eterm`
  (generic tagged pointer, masks `TAG_PTR_MASK` incl. `TAG_LITERAL_PTR`).
- `is_list`/`is_not_list`; `is_not_nil_list` (= `is_list`: NIL is an immediate, not a LIST word).
- `car(Eterm) -> Eterm`; `cdr(Eterm) -> Eterm` (raw 2-word cell access); `CAR_OFFSET=0`, `CDR_OFFSET=1`.

### `boxed.rs` — boxed pointers, header words, boxed sub-types
- `make_boxed(*mut Eterm) -> Eterm`; `boxed_val(Eterm) -> *mut Eterm`; `is_boxed`/`is_not_boxed`.
- Headers: `make_header(size, subtag)`, `header_subtag(Eterm) -> u32`, `header_arity(Eterm) -> usize`,
  `thing_arityval(Eterm) -> usize`.
- Arityval (tuple header): `make_arityval(usize) -> Eterm`, `make_arityval_zero()`,
  `make_arityval_unchecked(usize)`, `try_make_arityval(usize) -> TermResult<Eterm>`, `arityval(Eterm) -> usize`,
  `is_arity_value`, `is_sane_arity_value`.
- Sub-type predicates: `is_map`/`is_map_header`/`is_flatmap`/`is_hashmap`, `is_float`, `is_big`,
  `is_internal_ref`, `is_any_fun`, `is_record`, `is_bitstring`/`is_binary`,
  `is_external_pid`/`is_external_port`/`is_external_ref`.
- Map header field accessors: `MAP_HEADER_TYPE/ARITY/VAL(Hdr)` (per `erl_term.h`).

### `tuple.rs` — tuples
- `make_tuple(*mut Eterm) -> Eterm`; `tuple_val(Eterm) -> *mut Eterm`; `is_tuple`/`is_not_tuple`;
  `is_tuple_arity(Eterm, usize) -> bool`; `tuple_size(Eterm) -> usize`;
  `tuple_element(Eterm, usize) -> Eterm`; `try_tuple_element(Eterm, usize) -> TermResult<Eterm>`.
- `pub fn tuple0() -> Eterm` — the shared, `TAG_LITERAL_PTR`-tagged empty-tuple literal (backed by a
  `LazyLock` over one leaked literal word). Zero-arity heap tuples are discouraged.

### `heap.rs` — term backing store (bump arena)
The allocation interface this representation is built on. **Minimal and `unsafe`-marked by design**;
`beam_alloc` (production allocators/carriers) and `beam_gc` (per-process copying GC) extend/replace it —
see Design Decisions. Never reallocates, so handed-out pointers stay valid for the arena's lifetime.
- `pub struct Heap`; `Heap::new(capacity_words) -> Heap`; `capacity()`, `used()`, `available()`,
  `reset()`, `hp() -> *mut Eterm`, `hend() -> *mut Eterm`.
- `unsafe fn alloc_words(&mut self, n: usize) -> TermResult<*mut Eterm>` (uninitialised, contiguous).
- `cons(&mut self, car, cdr) -> TermResult<Eterm>`; `tuple(&mut self, elems: &[Eterm]) -> TermResult<Eterm>`;
  `unsafe fn alloc_tuple(&mut self, n: usize) -> TermResult<*mut Eterm>`.

### `literal.rs` — off-heap literal areas (`TAG_LITERAL_PTR`)
- `make_literal(Eterm) -> Eterm`; `is_literal(Eterm) -> bool`; `clear_literal(Eterm) -> Eterm`.
- `unsafe fn set_literal_tag(term: *mut Eterm, hp_start: *mut Eterm, hsz: usize)` — the
  `erts_set_literal_tag` graph walk (ORs `TAG_LITERAL_PTR` into every BOXED/LIST word + the root).
- `pub struct LiteralArea` — a bump-allocated, frozen term block (`start`/`end`/`off_heap`), with
  `alloc_words`, `contains(ptr)`, `is_literal(ptr)` (pointer-range test).

### `ops.rs` — cross-cutting classifiers
- `pub enum TermClass { Bitstring, List, Nil, Map, Record, Tuple, Pid, ExternalPid, Port,
  ExternalPort, Fun, Ref, ExternalRef, Atom, Float, Big, Small, BinRef }` — mirrors the C `*_DEF`
  virtual tag values (term order: number < atom < ref < fun < port < pid < tuple < record < map <
  nil < cons < binary) used by comparison/ordering and `terms` code.
- `unsafe fn class(Eterm) -> TermClass` (the `tag_val_def` equivalent; dereferences a boxed header).
- `unsafe fn is_integer(Eterm)` (= small ‖ big); `unsafe fn is_number(Eterm)` (= integer ‖ float).
- `THE_NON_VALUE: Eterm` (= `0` in release, `make_header(0, Float)` in debug); `is_non_value`,
  `is_value`, `is_zero_sized`.

### `compare.rs` — Erlang term ordering (`erts_cmp`)
The lowest-common-ancestor ordering shared by `beam_ets` (ordered_set), `beam_bifs` (comparison
operators) and `<`/`>` between same-typed terms.
- `pub fn cmp(a: Eterm, b: Eterm, atoms: &AtomTable) -> core::cmp::Ordering` — the exact (`=:=`)
  order, reproducing `erts_cmp(a, b, exact=1, eq_only=0)` / `erts_cmp_compound` (`utils.c`).
  Ascending order `number < atom < ref < fun < port < pid < tuple < record < map < nil < list <
  bitstring` (plus external variants), derived from the `_DEF` virtual tags in `erl_term.h`.
- `pub fn eq(a: Eterm, b: Eterm, atoms: &AtomTable) -> bool` — `cmp(a,b).is_eq()` (`=:=`).
- `pub fn cmp_arith(a: Eterm, b: Eterm, atoms: &AtomTable) -> core::cmp::Ordering` — the
  arithmetic (`==`) order, reproducing `erts_cmp(a, b, exact=0, eq_only=0)` (differs from `cmp`
  only for floats and mixed integer/float pairs).
- The bit/float payload helpers live in [`bitstring.rs`](#bitstringrs--bit-level-payloads); this
  module forwards to them so each layout has a single definition.
### `hashing.rs` — structural term hashing
- `pub fn term_hash(a: Eterm, atoms: &AtomTable) -> u64` — a deterministic structural hash
  reproducing the *properties* of `make_hash2` (`erl_term_hashing.c`): equal terms hash equal.
  **The exact hash values are internal and NOT part of any compatibility contract** (nothing
  outside the Rust runtime observes them); the only guarantee is `eq(a,b,atoms)` ⇒ equal hashes.

### `size.rs` — object word size
- `pub fn size_object(a: Eterm) -> usize` — the *flat* number of heap words the term graph
  occupies, reproducing `size_object_x(obj, NULL)` (`copy.c`): `0` for immediates, `2` per cons
  cell, `arity + 1` per boxed object (header + payload), plus the sizes of nested terms.

### `thing.rs` — off-heap term materialization (heap-agnostic)
Frozen-layout **writers and readers for the boxed "thing" families whose interior layout this
crate owns** — float, bignum, flat map, and the external pid/port/ref (`ExternalThing`) blocks.
Every writer is **heap-agnostic**: the caller allocates the words (via `beam_alloc` in production,
`heap::Heap` in tests) and passes `hp: *mut Eterm`; the writer fills the block and returns the
tagged root `Eterm`. All word counts and bit patterns are **byte-identical** to the
oracle-verified `beam_codec::term` (`make_float`/`make_big`/`make_flatmap`/`make_external_*`),
which forwards here (see Design Decisions). Readers are `unsafe` with a caller invariant
("`t` is a live thing of this kind") and return borrowed slices.
- Float: `pub const FLOAT_SIZE_WORDS: usize = 2`; `unsafe write_float(hp, value: f64) -> Eterm`
  (`[HEADER_FLONUM, bits]`, root `make_boxed(hp)`); `unsafe float_value(t) -> f64`;
  `unsafe float_bits(t) -> u64` (re-exports of the single public definitions in
  [`bitstring.rs`](#bitstringrs--bit-level-payloads)).
- Bignum: `pub const fn big_size_words(n_digits) -> usize` (`n_digits + 1`, `BIG_NEED_SIZE`);
  `unsafe write_big(hp, negative: bool, digits: &[u64]) -> Eterm` (header arity `n - 1`, one
  `ErtsDigit` = `u64` per word, least-significant-first, sign in `_BIG_SIGN_BIT`; `digits` must be
  non-empty); `unsafe big_parts<'a>(t) -> (bool, &'a [u64])`.
- Flat map: `pub const fn flatmap_size_words(n_pairs) -> usize` (`3` for the empty map, else
  `4 + 2n`); `unsafe write_flatmap(hp, pairs: &[(Eterm, Eterm)]) -> Eterm` (the C block
  `[arityval(n), k0..k(n-1), MAP_HEADER_FLATMAP, n, keys-tuple, v0..v(n-1)]`, root
  `make_boxed(hp + 1 + n)`; `n_pairs ≤ MAP_SMALL_MAP_LIMIT`); `unsafe flatmap_parts<'a>(t) ->
  (&'a [Eterm], &'a [Eterm])`.
- External things (C `ExternalThing`, incl. the M1 "`next` slot holds `creation`" substitution):
  `pub const MAX_REF_IDS: usize = 5` (`ERTS_MAX_REF_NUMBERS`); the size fns
  `external_pid_size_words()` (`4`), `external_port_size_words()` (`4`),
  `external_ref_size_words(n_ids)` (`3 + n_ids/2 + 1` on `ARCH_64`); the writers
  `unsafe write_external_pid(hp, node, number: u32, serial: u32, creation: u32)`,
  `unsafe write_external_port(hp, node, number: u64, creation: u32)`,
  `unsafe write_external_ref(hp, node, ids: &[u32], creation: u32)` and their `unsafe *_parts`
  inverses (`external_pid_parts`/`external_port_parts`/`external_ref_parts`).

### `bitstring.rs` — bit-level payloads
The `ErlHeapBits` / `ErlSubBits` block writers and readers plus the crate's public bit/float payload
helpers (ground truth `../../erts/emulator/beam/erl_bits.h:60-176`, `erl_bits.c:490-519`). Bits are
**MSB-first inside each byte** (so `<<1:3>>` is the byte `0b0010_0000`); the unused trailing bits of
the last byte are zeroed, matching `beam_codec`'s oracle-verified encoder.
- `pub const fn bit_count_to_bytes(n_bits) -> usize` (`NBYTES`, `ceil(n_bits/8)`, overflow-free);
  `pub const fn heap_bits_size_words(n_bits) -> usize` (`2 + ceil(ceil(n_bits/8)/8)`,
  `heap_bits_size`); `pub const ERL_SUB_BITS_FLAG_MASK = 0x3`,
  `ERL_SUB_BITS_FLAG_MUTABLE`/`ERL_SUB_BITS_FLAG_VOLATILE`,
  `BITSTRING_POINTER_FLAG_MASK` (alias of the flag mask).
- `unsafe write_heap_bits(hp, bytes: &[u8], n_bits) -> TermResult<Eterm>` — the `ErlHeapBits`
  writer (`[make_header(words-1, HEAP_BITS_SUBTAG), n_bits, data…]`, root `make_boxed(hp)`); a
  `bytes` shorter than `ceil(n_bits/8)` yields `TermError::BadBitstring`.
- `unsafe write_binary(hp, bytes: &[u8]) -> TermResult<Eterm>` — a bitstring of `bytes.len()*8` bits.
- `unsafe write_sub_bits(hp, base_flags, start, end, orig) -> Eterm` and
  `unsafe sub_bits_parts(t) -> (Eterm, Eterm, Eterm, Eterm)` — the 5-word `ErlSubBits` block.
- `unsafe bitstring_parts(t) -> (*const u8, usize, usize)` (data base, **bit** offset, **bit** size);
  `unsafe bit_at(t, i) -> bool`.
- `unsafe float_bits(t) -> u64` / `unsafe float_value(t) -> f64` — the crate's single public float
  payload readers (`compare`/`hashing`/`thing` forward/re-export them).

### `refc.rs` — off-heap (`refc`) binaries
The on-heap `BinRef` + `ErlSubBits` **pair** for a binary/bitstring larger than the on-heap limits
(ground truth `../../erts/emulator/beam/erl_bits.h:135-176`, `erl_bits.c:2672-2712`). The 8-word
arrangement is `[BinRef(3)][ErlSubBits(5)]` with the tagged root at `hp + 3` (the `ErlSubBits`).
`beam_term` cannot allocate the off-heap payload, so every writer is heap-agnostic over a caller
`hp`/`val`.
- `pub const fn bin_ref_size_words() -> usize` (3), `pub const fn sub_bits_size_words() -> usize` (5),
  `pub const ERL_REFC_BITS_SIZE = 8`, `pub const ERL_ONHEAP_BINARY_LIMIT = 64`,
  `pub const ERL_ONHEAP_BITS_LIMIT = 512`.
- `unsafe write_bin_ref(hp, val, next) -> Eterm`; `unsafe bin_ref_parts(t) -> (*mut Eterm, *mut Eterm)`.
- `unsafe write_refc_binary(hp, val, next, base, offset_bits, size_bits) -> Eterm` — the full pair.
- `unsafe is_refc_binary(t) -> bool`, `unsafe refc_offset(t) -> usize` (**bits**),
  `unsafe refc_size(t) -> usize` (**bits**, `end - start`),
  `unsafe refc_bytes<'a>(t) -> &'a [u8]` (`ceil((offset+size)/8) - offset/8` bytes of the payload).
- **Frozen inter-crate contract (`beam_alloc`/`beam_gc`):** `BinRef.val` holds a raw pointer to the
  payload's **first data byte** (the off-heap object header is `beam_alloc`'s concern);
  `ErlSubBits.orig` is a term (a boxed `BinRef`, or an on-heap bitstring for a match context); the 8
  words are moved/copied together with `orig` fixed up to the new `BinRef` address.

### `map.rs` — flat map + HAMT/CHAMP builder, lookups, structural size
The crate's second frozen interior: the Erlang **map**. A map is a boxed `MAP_SUBTAG` term in one of
two shapes (ground truth `../../erts/emulator/beam/erl_map.h`, `erl_map.c`):
a **flat map** (`MAP_HEADER_FLATMAP`, written by [`thing.rs`](#thingrs--off-heap-term-materialization))
for ≤ [`MAP_SMALL_MAP_LIMIT`] pairs, else a **hash map** — a 16-way (`4` hash bits/level,
`HAMT_MAX_LEVEL = 16`) compressed hash-array-mapped trie. Trie node layouts (frozen, exactly what
`beam_gc::copy::move_size` sizes): head-array `[hdr][size][16 slots]` (18 words), head-bitmap
`[hdr(bmp)][size][n slots]` (`2+n`), inner bitmap `[hdr(bmp)][n slots]` (`1+n`), collision node = a
`make_arityval(n)` tuple of `n` key/value cons cells. One **term per slot**: a leaf cons cell, a
pointer to a sub-node, or a collision tuple. Level `L` selects `nibble(hash, L) = (hash >> (4*(15-L)))
& 0xF` (MSB nibble indexes the root).
- `pub const MAP_SMALL_MAP_LIMIT: usize = 32;` (`erl_map.h:96`); `pub const HAMT_MAX_LEVEL: usize = 16;`
- `pub const fn make_map_header(map_type, arity, val) -> Eterm` (`MAKE_MAP_HEADER`);
  `pub const fn map_words_capacity(n_pairs) -> usize` (exact for a flat map, sound upper bound for a
  hash map).
- `unsafe write_map(hp, pairs: &[(Eterm,Eterm)], atoms) -> TermResult<Eterm>` — dedups (last value
  wins), canonical order, flat map when ≤ limit else a hash map; leaves + trie are laid out in
  canonical (hash-then-`cmp`) order so the buffer is insertion-order free.
- `map_size(t, atoms) -> TermResult<usize>`, `map_get(t, key, atoms) -> TermResult<Option<Eterm>>`,
  `map_pairs(t, atoms) -> TermResult<Vec<(Eterm,Eterm)>>` (sorted by [`compare::cmp`]),
  `unsafe map_put(hp, t, key, val, atoms)` / `unsafe map_remove(hp, t, key, atoms)` (functional).
- `pub(crate) map_pairs_flatmap_order(t, atoms)` — pairs in internal flat-map key order (the order
  `compare`/`hashing` use so a flat map and a hash map of the same pairs present identically).
- Non-boxed / non-map terms yield `TermError::BadMap` (never a deref of an immediate).

## Constraints
- **Bit-exact layout (Level-1 drop-in).** Every constant, mask, shift and encoding must match
  `../../erts/emulator/beam/erl_term.h` exactly; a change here breaks the frozen ABI and every crate.
- **Dependencies:** `beam_common` + `libc` only (this crate is a foundation dependency sink). Nothing
  else. `beam_common` is used for exactly one thing: the `From<TermError> for beam_common::BeamError`
  impl in `error.rs` (owned here so every dependent gets `?`-interop for free); `TermError`/
  `TermResult` remain this crate's error types and must not be redefined per dependent.
- **`unsafe` confinement:** confined to `heap`, `literal`, `list`, `boxed`, `tuple`, `thing`, `map`,
  `compare`, `hashing`, `bitstring`, `refc` (raw pointer deref). Every `unsafe fn` documents a
  `# Safety` section; every `unsafe` block carries a
  `// SAFETY:` rationale. `unsafe_op_in_unsafe_fn` stays clean (workspace lint).
- **Pure tag math is `const fn`** where the body allows (`raw`, `from_raw`, `primary_tag`, tag tests,
  `make_small`, `signed_value`, `make_atom`, `atom_val`, `make_arityval`, `arityval`, …) — these are
  hot in the engine.
- Files target ~1000 lines; one responsibility per file.

## Design Decisions
- **`Eterm` = `#[repr(transparent)]` newtype over `usize`**, not a raw `usize` alias: gives type
  safety, methods, and a documented ABI contract while remaining layout/pass-identical to
  `ERL_NIF_TERM` (Level-1 drop-in requires this — Part I §A.3).
- **Checked + unchecked constructors.** `make_small`/`make_arityval` are unchecked (they are the
  engine hot path, mirroring C), but `try_*` variants return `TermResult` for use at trusted
  boundaries (arithmetic BIFs, loader, `beam_alloc`) so range violations become errors, not silent
  corruption (workspace error strategy §3: no silent swallowing).
- **`Atom` is `u32`.** The representation allows 58-bit atom indices; the practical atom table is
  capped far below 2^32 (default `+t` limit 1,048,576), so a 32-bit index is compact and sufficient.
  Atoms are only ever produced via `make_atom(Atom)`, so the encoding invariant always holds.
- **`AtomTable` assigns indices in insertion order (0-based).** This reproduces `atom.names` exactly
  (verified against `../../erts/emulator/utils/make_tables`: `#define am_<alias> make_atom($i)` with
  `$i` the 0-based file order), which is required for preloaded modules that reference atoms
  positionally.
- **`heap::Heap` is a minimal term-construction arena, NOT the production allocator.** The workspace
  routing table assigns the production per-process heap / bump allocator to `beam_alloc`. This module
  exists (per the node objective) only so the representation has a concrete, testable backing store
  for constructing boxed/list terms; `beam_alloc`/`beam_gc` build the real thing on top. It never
  reallocates (fixed capacity) so interior pointers never dangle.
- **`TUPLE0` is `tuple0()`, a function, not a `const`.** The spec asked for `pub const TUPLE0: Eterm`,
  but a shared empty-tuple literal must carry `TAG_LITERAL_PTR`, which requires forming a pointer to a
  static block — and Rust rejects pointer→integer casts during const evaluation. It is therefore
  exposed as `tuple0()`. Same root cause (no const pointer arithmetic): `make_boxed`/`make_list`/
  `make_tuple` are plain `fn`, not `const fn`.
- **Off-heap term materialization lives here (`thing.rs`), heap-agnostic over a caller `*mut Eterm`.**
  The interior layout of a float/bignum/flatmap/external thing is owned by this crate (the
  frozen-layout lowest common ancestor of every term-producing crate — workspace §2), so its
  constructors/accessors belong here rather than duplicated in `beam_codec`. This crate **cannot
  depend on `beam_alloc`** (it is a dependency sink; `beam_alloc` already depends on `beam_term`, so
  the reverse would cycle), which is exactly why every writer takes a caller-allocated
  `hp: *mut Eterm` and returns the tagged root instead of allocating. The codec keeps only its
  error-wrapping + allocation and forwards the word layout here (removing the duplication);
  `float_bits`/`float_value` are re-exports of the single public float readers in
  [`bitstring.rs`](#bitstringrs--bit-level-payloads) so the float-layout knowledge has a single
  definition.
- **The bit/float payload readers live in `bitstring.rs`.** `bitstring_parts`, `bit_at`,
  `float_bits` and `float_value` are defined once, publicly, in `bitstring.rs`; `compare.rs` and
  `hashing.rs` forward to them (via `use`) and `thing.rs` re-exports `float_bits`/`float_value`.
  Keeping one definition avoids the `ambiguous_glob_reexports` hazard of two modules exporting the
  same name, and removes the previous `pub(crate)`-only duplication between `compare.rs` and the
  deleted `beam_codec` bitstring shim.

## Test Strategy
- Per-module `#[cfg(test)] mod tests` unit tests.
- `tests/layout.rs` — **pins the exact raw bit patterns** against `erl_term.h` (e.g. `make_small(0)
  == 0xF`, `make_atom(0) == 0xB`, `NIL == 0x3B`, `make_arityval(1) == 0x40`, order-`List` cons tag
  `0x1`), plus boundary sweeps: small-int min/max/±1/0, `arityval` at `MAX_ARITYVAL`, literal-bit
  masking via `boxed_val`/`list_val`. Prefer property-style loops over boundary ranges.
- `tests/abi.rs` — `size_of::<Eterm>() == size_of::<usize>()`, `align_of` equal, `Eterm: Copy`, and a
  `repr(transparent)` assertion (round-trip `Eterm`) — the NIF-ABI guarantee.
- Behavioural tests: atom interning/lookup/name round-trip + `atom.names` parser on an embedded
  snippet (order + `CName='printname'` + quoted names), heap cons/tuple construction + car/cdr +
  exhaustion, literal-graph tagging walk (`set_literal_tag` on a nested tuple/list).
- `src/compare.rs`, `src/hashing.rs`, `src/size.rs` carry their own in-module `#[cfg(test)]` suites:
  ordering across every class pair (number < atom < ref < port < pid < tuple < map < nil < list <
  binary), atom name ordering, tuple/list elementwise + size tie-break, numeric int/float ordering,
  structural-hash equality for equal terms (and inequality for different terms), and `size_object`
  for immediates / cons / tuple / boxed. Prefer property-style boundary loops.
- `src/thing.rs` carries its own `#[cfg(test)]` suite pinning each block's raw words against the
  legacy headers (float `[HEADER_FLONUM, bits]`; bignum header arity `n-1` + LS-first `u64` digits
  and the sign bit; flatmap `[arityval(n), k.., MAP_HEADER_FLATMAP, raw size, keys-tuple, v..]` plus
  the 3-word empty block; the external pid/port/ref head + data words), plus write→read round-trips
  and boundaries (min/max `MAX_REF_IDS`, single-digit bignum, empty flatmap, negative bignum).
- `src/bitstring.rs` carries its own suite: `bit_count_to_bytes`/`heap_bits_size_words` boundary
  values (0/1/8/9/64/65/512/513 …), the `ErlHeapBits` header/word pin, non-byte-aligned sizes
  `1..=64` round-tripped bit-by-bit (MSB-first, trailing bits zeroed), the too-short byte count
  (`TermError::BadBitstring`), the `ErlSubBits` word pin + flag masking, and the float readers.
- `src/refc.rs` carries its own suite: the size/limit constants + `HEADER_BIN_REF`/`HEADER_SUB_BITS`
  raw words (`0xA8`/`0x124`), the `BinRef` word pin, the pinned 8-word combined pair
  (`[BinRef][ErlSubBits]`, root at `hp + 3`), windowed `refc_offset`/`refc_size`/`refc_bytes`, and
  `is_refc_binary` rejecting on-heap bitstrings, non-bin-ref sub-bitstrings, immediates and tuples.
- `tests/oracle.rs` — the crate-local **oracle cross-check**: spawns the system `erl`
  (`-noshell -eval`, text protocol — no ETF re-implementation, since dev-depending on `beam_codec`
  would cycle) and checks that `beam_term`'s map/bitstring *observables* agree with the real
  runtime: `map_size`/`map_get`/`map_pairs` vs `maps:size`/`maps:get`/`lists:sort(maps:keys/1)`
  across the flat/hash boundary (31/32/33/40/100 keys) and for atom-keyed maps, and
  `bitstring_parts`/`bit_at` vs `bit_size`/per-bit/byte views for `<<1:3>>`-style bitstrings and
  binaries. Skips loudly (never fails) when `erl` is absent/does not answer.
- Exhaustive cross-checks against the real system ERTS belong to `tools/differential` (the
  compatibility gate); `tests/oracle.rs` is only the crate-local map/bitstring smoke slice.
- Iteration commands (read-only `$HOME`): `../../scripts/cargo.sh test -p beam_term` (GREEN),
  `../../scripts/cargo.sh build -p beam_term`. Never `cd`.

## Routing Table
- `src/tag.rs` → raw tag constants/masks/shifts, `PrimaryTag`/`BoxedTag`, `primary_tag`, `is_header`
- `src/eterm.rs` → the `Eterm` word newtype (`raw`/`from_raw`), ABI contract
- `src/immediate.rs` → small ints, atom immediates, NIL, catch, pid/port immediates
- `src/atom.rs` → `Atom` index type, `AtomTable` (interning, `atom.names` seeding/parser)
- `src/list.rs` → cons-cell construction/access (`make_list`, `car`, `cdr`, `is_list`)
- `src/boxed.rs` → boxed pointers, header words, arityval, boxed sub-type predicates, map headers
- `src/tuple.rs` → tuple construction/size/element access
- `src/heap.rs` → minimal bump term arena (the representation's allocation interface)
- `src/literal.rs` → `TAG_LITERAL_PTR` tagging + literal-area graph walk + `LiteralArea`
- `src/ops.rs` → `TermClass`/`class`, numeric predicates, `THE_NON_VALUE`
- `src/compare.rs` → Erlang term ordering (`cmp`/`eq`/`cmp_arith`), the `erts_cmp` port + fallbacks
- `src/hashing.rs` → structural term hashing (`term_hash`), the `make_hash2`-property port (internal)
- `src/size.rs` → object word size (`size_object`), the `size_object_x` port
- `src/thing.rs` → heap-agnostic writers + readers for float/bignum/flatmap/external-pid/port/ref blocks (frozen off-heap layout; the codec forwards here)
- `src/map.rs` → the Erlang map: flat-map/HAMT (`CHAMP`) builder (`write_map`), `map_size`/`map_get`/`map_put`/`map_remove`/`map_pairs`, `MAP_SMALL_MAP_LIMIT`, `make_map_header`/`map_words_capacity` (frozen map node layout)
- `src/bitstring.rs` → bit-level payloads: `ErlHeapBits`/`ErlSubBits` writers + readers, `bit_count_to_bytes`/`heap_bits_size_words`, `bitstring_parts`/`bit_at`, the public `float_bits`/`float_value`
- `src/refc.rs` → off-heap (`refc`) binaries: the `BinRef` + `ErlSubBits` on-heap pair writers/accessors, `ERL_REFC_BITS_SIZE`/`ERL_ONHEAP_*` limits, `is_refc_binary`/`refc_*`
- `src/error.rs` → local `TermError`/`TermResult` + the `From<TermError> for beam_common::BeamError`
  interop impl (upstream reconciliation so dependents get `?`-interop)
- `tests/` → crate-level integration tests (bit-pattern pins `layout.rs`, ABI pins `abi.rs`, system-`erl` oracle cross-check `oracle.rs`)

## Known Issues
- **Comparison gaps (`src/compare.rs`).** `cmp`/`eq`/`cmp_arith` faithfully reproduce
  `erts_cmp_compound` for atoms (by name), numbers (small/big/float), tuples, lists, bitstrings, maps
  (flat *and* hash — size first, then the `(key,value)` sequence in flat-map key order) and
  internal pids/ports. The following classes use a **deterministic, panic-free fallback** rather than
  a structural comparison and must be extended if exact ordering is needed: big integers, external
  pid/port/ref, funs, records and on-heap bin refs (ordered by class rank, then object
  arity, then raw term word); internal references (compared by raw payload words, not the decoded
  `num[]`); atoms whose name is absent from the passed `AtomTable` (fall back to the raw atom index).
  Hash-map comparison cannot be byte-matched to ERTS (ERTS's own HAMT order depends on its hash) — the
  flat-map-key-order rule is used for both shapes (see `src/map.rs` docs).
- **Hashing is internal, not compatibility-pinned (`src/hashing.rs`).** `term_hash` guarantees only
  the hash/equality consistency rule `eq(a,b,atoms) ⇒ term_hash(a,atoms) == term_hash(b,atoms)`; its
  exact bit values are not part of any contract. Maps hash structurally (their sorted pairs). Classes
  lacking a layout-aware representation (funs, records, external pid/port/ref, bin refs) use an opaque
  raw-word fallback, so their hash is not stable across processes/images.
- **Object-size approximations (`src/size.rs`).** Hash maps are walked node-by-node (head
  array/bitmap, interior bitmaps, collision nodes, flat-map leaves). Sub-bitstrings report the on-heap
  thing size rather than the copied representation; native records are treated like tuples.
- **Hash choices are internal (`src/map.rs`).** The HAMT trie is driven by `hashing::term_hash`, not
  `erts_map_hash`, so a node's *bitmap* for a given key set is not `erts_map_hash`-identical. The node
  *encodings* (headers, sizes, slot semantics, collision/flat-map formats, key order) are bit-exact to
  ERTS; nothing outside the Rust runtime observes those bitmaps.

## Notes for Agents
- **Read `../../erts/emulator/beam/erl_term.h` before changing any constant** — it is the ground truth;
  cite it in comments where a value is non-obvious.
- The C `TAG_LITERAL_PTR` (0x4) is enabled on 64-bit when there is no OS physical-memory reservation
  (the common case) or under DEBUG; we always define it (Level-1 fidelity) and mask it in
  `boxed_val`/`list_val` via `TAG_PTR_MASK` (0x7).
- `is_list(NIL)` is **false** — NIL is an `IMMED2` immediate; a "list that is not nil" is exactly a
  cons cell (LIST-tagged word). `List` terms are single words (must stay so for the NIF/ETF/msg ABI).
- `make_arityval` encodes `size << 6` with subtag `0x00`, so an arity of 0 encodes to the raw word
  `0`; table cells/`[Eterm; N]` register files must therefore not treat `0` as "empty" without care.
- Interior layouts that are NOT externally pinned (list chunking, map flatmap/HAMT thresholds, ETS
  tree shape) are open and documented at their own nodes — this crate only owns what the frozen
  layout requires.

## See Also
- `../../CONTEXT.md` — workspace contracts (§2 frozen term layout, §3 error strategy).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.1–A.3, §B; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A, §B.
- `../../erts/emulator/beam/CONTEXT.md` — legacy core; its "Term Encoding (Eterm) Reference" section.
- `../beam_abi/` — NIF/driver C ABI; transmutes `Eterm` ↔ `ERL_NIF_TERM`.
- `../beam_alloc/`, `../beam_gc/` — production heap/allocator + collector that build on `heap.rs`.
