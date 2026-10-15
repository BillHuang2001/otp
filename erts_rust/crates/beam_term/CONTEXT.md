# `beam_term` — Frozen BEAM Term Representation

## Intent
Owns the **frozen, bit-exact tagged term representation** of the Rust ERTS rewrite — the most
load-bearing shared contract in the workspace (workspace `CONTEXT.md` §2) — at **Level-1 drop-in
fidelity**: a Rust core and the retained C periphery must not differ by one bit. Ground truth is the
legacy C (`../../erts/emulator/beam/erl_term.h`, `erl_term.c`); every other crate depends downward on
this one for term construction, inspection and classification. This node is the lowest common ancestor
for term operations: **no other crate may assume a different layout.** Implements Part I §A.1–A.2
(language semantics + `.beam` data model) and the term-layout discussion of Part II §B.

## API Surface (FROZEN — every downstream crate routes here)

`Eterm` is a `#[repr(transparent)]` newtype over `usize`, ABI-identical to a NIF's `ERL_NIF_TERM`
(both one machine word). `beam_abi` transmutes between them; no other crate may redefine it.

### crate root (`src/lib.rs`)
`pub mod {tag, eterm, immediate, atom, list, boxed, tuple, heap, literal, ops, compare, hashing, size,
layout, thing, bitstring, refc, map, fun, record, number, error}` + `pub mod prelude` + flat `pub use`
re-exports of every public item, so downstream code can `use beam_term::prelude::*;`.

### `eterm.rs` — the word type
- `pub struct Eterm(usize)` — `#[repr(transparent)]`; `Copy + Clone + PartialEq + Eq + Hash + Debug`.
- `pub const fn Eterm::raw(self) -> usize`; `pub const fn Eterm::from_raw(raw: usize) -> Eterm`.
- `impl Default for Eterm` → `NIL` (BEAM's "blank = NIL" convention). `pub const fn Eterm::nil() -> Eterm`;
  `pub const fn Eterm::non_value() -> Eterm`.

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
- `pub enum PrimaryTag { Header, List, Boxed, Immed1 }`.
- `pub enum BoxedTag { ArityVal=0x00, PosBig=0x08, NegBig=0x0C, Ref=0x10, Fun=0x14, Float=0x18,
  Record=0x1C, HeapBits=0x20, SubBits=0x24, BinRef=0x28, Map=0x2C, ExternalPid=0x30,
  ExternalPort=0x34, ExternalRef=0x38 }` (+ `BoxedTag::from_raw(subtag)`).
- `pub const fn primary_tag(Eterm) -> PrimaryTag`; `pub const fn is_header(Eterm) -> bool`;
  `pub const fn is_term_word(Eterm) -> bool` (= `!is_header`).

### `immediate.rs` — immediates
- Small ints: `SMALL_BITS=60`, `MAX_SMALL=(1<<59)-1`, `MIN_SMALL=-(1<<59)`; `is_immediate`/
  `is_not_immediate`; `is_small`/`is_not_small`; `make_small(i64) -> Eterm` (unchecked, `debug_assert`
  in range); `try_make_small(i64) -> TermResult<Eterm>`; `unsigned_value(Eterm) -> u64`;
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
- `AtomTable::new() -> AtomTable`; `intern(&mut self, name: &str) -> Atom`; `lookup(&self, name: &str)
  -> Option<Atom>`; `name(&self, Atom) -> Option<&str>`; `len`, `is_empty`.
- `seed(&mut self, ordered_names: impl IntoIterator<Item = &str>)` (pre-intern in order);
  `seed_from_atom_names(&mut self, src: &str) -> TermResult<()>` (parses the `atom.names` grammar:
  `atom` <name>+ ; names bare / `'quoted'` / `CName='printname'`; `#` comments; interns in file order);
  `from_atom_names(src) -> TermResult<AtomTable>`.
- `hvalue(&self, a: Atom) -> u32` — the C `atom_tab(a)->slot.bucket.hvalue`: Dragon-Book `hashpjw` over
  the atom's UTF-8 name bytes with the latin1 clutch (`atom.c:82` `atom_hash`). This is the value
  `make_hash`/`make_hash2` mix for an atom; an out-of-range index falls back to `a.raw()` (documented,
  panic-free).
- Generating the standard `am_*`-equivalent `Atom` constants from the full `atom.names` is a
  codegen/boot step routed to `beam_rt`; this crate owns only the mechanism + parser.
- **O(1) append-only witness** (full contract under [Atom-table snapshots](#atom-table-snapshots--the-o1-append-only-memo-witness)):
  `generation(&self) -> u64` (bumped by exactly one per **newly** interned name),
  `snapshot(&self) -> AtomSnapshot` (opaque `Copy` token carrying table identity + generation +
  length), `appended_since(&self, AtomSnapshot) -> bool` (O(1), allocation-free; `true` only when
  `self` is the same instance as the snapshotting table and every mutation since was a pure append).
  `AtomSnapshot` exposes `len()`/`is_empty()`/`generation()`.
- **Backing store & cost.** `AtomTable` is `{ names: Vec<String>, index: HashMap<String, Atom>,
  instance: u64, generation: u64 }` (the two extra scalars exist only for the witness); derives
  `Debug`, hand-writes `Default`/`Clone`. `name(Atom)` is `O(1)`; `intern` is one `HashMap` lookup that
  allocates **nothing** for an already-interned name (no generation bump) and **two** `String`s for a
  new one. `clone()` is a **deep copy of every entry and mints a fresh `instance`** — `O(N)`
  allocations; never clone a populated table on a hot path.

### `list.rs` — cons cells (`[CAR,CDR]`, header-less, `TAG_PRIMARY_LIST`)
- `make_list(*const Eterm) -> Eterm`; `list_val(Eterm) -> *mut Eterm`; `ptr_val(Eterm) -> *mut Eterm`
  (generic tagged pointer, masks `TAG_PTR_MASK` incl. `TAG_LITERAL_PTR`).
- `is_list`/`is_not_list`; `is_not_nil_list` (= `is_list`: NIL is an immediate, not a LIST word).
- `car(Eterm) -> Eterm`; `cdr(Eterm) -> Eterm` (raw 2-word cell access); `CAR_OFFSET=0`, `CDR_OFFSET=1`.

### `boxed.rs` — boxed pointers, header words, boxed sub-types, the internal reference
- `make_boxed(*mut Eterm) -> Eterm`; `boxed_val(Eterm) -> *mut Eterm`; `is_boxed`/`is_not_boxed`;
  `is_boxed_term` (boxed *and* a term word).
- Headers: `make_header(size, subtag)`, `header_subtag(Eterm) -> u32`, `header_arity(Eterm) -> usize`,
  `thing_arityval(Eterm) -> usize`.
- Arityval: `make_arityval(usize) -> Eterm`, `make_arityval_zero()`, `make_arityval_unchecked(usize)`,
  `try_make_arityval(usize) -> TermResult<Eterm>`, `arityval(Eterm) -> usize`, `is_arity_value`,
  `is_sane_arity_value`.
- Sub-type predicates: `is_map`/`is_map_header`/`is_flatmap`/`is_hashmap`, `is_float`, `is_big`,
  `is_internal_ref`, `is_any_fun`, `is_record`, `is_bitstring`/`is_binary`,
  `is_external_pid`/`is_external_port`/`is_external_ref`, `is_external_fun`/`is_imported_fun`/`is_local_fun`.
- **M1 internal-reference thing — the canonical layout** (this crate's single documented truth for an
  on-heap reference): the frozen 4-word `[make_header(3, REF_SUBTAG), node, serial, id]` block, declared
  by `INTERNAL_REF_PAYLOAD_WORDS = 3` / `INTERNAL_REF_WORDS = 4`, written by
  `unsafe fn write_internal_ref(hp: *mut Eterm, id: u64) -> TermResult<Eterm>` (heap-agnostic: caller
  reserves the words, writer returns `make_boxed(hp)`; `TermError::SmallIntOutOfRange` when `id` is not
  a 60-bit small) and read by `unsafe fn internal_ref_id(t: Eterm) -> TermResult<u64>`
  (`TermError::BadRef` for a non-term, another boxed kind, a *C-layout* arity-2 block, or a non-small id
  word). These two are the only sanctioned producer/consumer pair for an internal reference.
- `unsafe fn internal_ref_numbers<'a>(t: Eterm) -> &'a [u32]` — **the C-layout reader** (`erl_term.h`):
  the `ErtsORefThing` `num[]` ids of a **3-word** `[make_header(2, REF_SUBTAG), w1, w2]` block. Kept as
  the C-layout description only; nothing calls it, and it must **never** be fed an M1 reference (an M1
  header passes `is_internal_ref` too, but its payload is three *terms*, so the read would return
  small-int tags instead of ids).
- Map header field accessors: `MAP_HEADER_TYPE/ARITY/VAL(Hdr)` (per `erl_term.h`).
- `make_record_header(size)` = `make_header(size, RECORD_SUBTAG)` (the record instance header).

### `tuple.rs` — tuples
- `make_tuple(*mut Eterm) -> Eterm`; `tuple_val(Eterm) -> *mut Eterm`; `is_tuple`/`is_not_tuple`;
  `is_tuple_arity(Eterm, usize) -> bool`; `tuple_size(Eterm) -> usize`; `tuple_element(Eterm, usize) ->
  Eterm`; `try_tuple_element(Eterm, usize) -> TermResult<Eterm>`.
- `pub fn tuple0() -> Eterm` — the shared, `TAG_LITERAL_PTR`-tagged empty-tuple literal (a `LazyLock`
  over one leaked literal word). Zero-arity heap tuples are discouraged.

### `heap.rs` — term backing store (bump arena)
Minimal and `unsafe`-marked by design; `beam_alloc` (production allocators/carriers) and `beam_gc`
(per-process copying GC) extend/replace it. Never reallocates, so handed-out pointers stay valid for the
arena's lifetime (`src/heap.rs`).
- `pub struct Heap`; `Heap::new(capacity_words) -> Heap`; `capacity()`, `used()`, `available()`,
  `reset()`, `hp() -> *mut Eterm`, `hend() -> *mut Eterm`.
- `unsafe fn alloc_words(&mut self, n) -> TermResult<*mut Eterm>` (uninitialised, contiguous);
  `cons(&mut self, car, cdr) -> TermResult<Eterm>`; `tuple(&mut self, elems: &[Eterm]) ->
  TermResult<Eterm>`; `unsafe fn alloc_tuple(&mut self, n) -> TermResult<*mut Eterm>`.

### `literal.rs` — off-heap literal areas (`TAG_LITERAL_PTR`)
- `make_literal(Eterm) -> Eterm`; `is_literal(Eterm) -> bool` (a bare bit test for `0x4`, meaningful
  **only** on BOXED/LIST words); `clear_literal(Eterm) -> Eterm`.
- `unsafe fn set_literal_tag(term: *mut Eterm, hp_start: *mut Eterm, hsz: usize)` — the
  `erts_set_literal_tag` graph walk (ORs `TAG_LITERAL_PTR` into every BOXED/LIST word + the root); a thin
  raw-pointer shim over `tag_literal_area`.
- `tag_literal_area(area: &mut [Eterm], roots: &mut [Eterm])` — the **safe, slice-based** form (no raw
  pointers/`unsafe`): tags every BOXED/LIST word of the contiguous `area` (stepping over HEADER bodies,
  with the `HEADER_SUB_BITS` `orig` special case, leaving IMMED1 words untouched), then each `roots` word
  that `is_boxed`/`is_list`. Shares the private `tag_words` index walk. See `src/literal.rs`.
- `pub struct LiteralArea` — a bump-allocated frozen term block (`start`/`end`/`off_heap`) with
  `alloc_words`, `contains(ptr)`, `is_literal(ptr)` (pointer-range test).

### `ops.rs` — cross-cutting classifiers
- `pub enum TermClass { Bitstring, List, Nil, Map, Record, Tuple, Pid, ExternalPid, Port, ExternalPort,
  Fun, Ref, ExternalRef, Atom, Float, Big, Small, BinRef }` — mirrors the C `*_DEF` virtual tags (term
  order number < atom < ref < fun < port < pid < tuple < record < map < nil < cons < binary).
  `NonValue` is the non-term class.
- `unsafe fn class(Eterm) -> TermClass` (the `tag_val_def` equivalent); `unsafe fn is_integer(Eterm)`
  (= small ‖ big); `unsafe fn is_number(Eterm)` (= integer ‖ float).
- `THE_NON_VALUE: Eterm` (= `0` in release, `make_header(0, Float)` in debug); `is_non_value`,
  `is_value`, `is_zero_sized`.

### `compare.rs` — Erlang term ordering (`erts_cmp`)
The LCA ordering shared by `beam_ets` (ordered_set), `beam_bifs` (comparison operators) and `<`/`>`
between same-typed terms.
- `pub fn cmp(a, b, atoms: &AtomTable) -> core::cmp::Ordering` — the exact (`=:=`) order, reproducing
  `erts_cmp(a, b, exact=1, eq_only=0)` / `erts_cmp_compound` (`utils.c`). Ascending `number < atom < ref
  < fun < port < pid < tuple < record < map < nil < list < bitstring` (plus external variants), from the
  `_DEF` virtual tags.
- `pub fn eq(a, b, atoms) -> bool` — `cmp(a,b).is_eq()` (`=:=`).
- `pub fn cmp_arith(a, b, atoms) -> core::cmp::Ordering` — the arithmetic (`==`) order, reproducing
  `erts_cmp(a, b, exact=0, eq_only=0)` (differs from `cmp` only for floats and mixed integer/float
  pairs). A mixed integer/float pair uses the C's **exact** algorithm, not an `f64` conversion (the
  `mixed_types` block, `utils.c:2345-2460`), so `1 == 1.0`, `-0.0 == +0.0` and
  `9007199254740993 > 9007199254740992.0` all hold; NaN/±inf deterministic.
- **Same-class structural arms:** `cmp`/`eq` compare external pids, external ports, external references
  and native records field-by-field in the C's `erts_cmp` order, so a **copy of such a term is `=:=` its
  original** (the copy boundary's identity check): pid `serial → number → node(sysname, creation)`; port
  `node(sysname, creation) → number`; reference `node → ids` as a bignum with the highest-index zero
  words trimmed; record `module → name → is_exported(false<true) → field count → keys → values`. Records
  are always compared structurally (the C's definition-pointer short-circuit has no counterpart here).

### `compare/numbers.rs` — exact mixed integer/float comparison
`pub(super) fn cmp_numbers(...)` (consumed by `compare.rs`): the `mixed_types` port of
`erts_cmp_compound` (`utils.c:2345-2460`) plus `erts_float_comp`, plus the crate's private `double_to_big`
(exact integral-`f64` → LS-first `u64` limbs, `big.c:2536`). `Small,Big`/`Big,Small` decide by bignum
sign; `Small,Float`/`Float,Small` compare inside the lossless `±(2^53−2)` window else the C's exact
integer subtraction; `Big,Float`/`Float,Big` walk the C's magnitude bands, the lossless `big_to_f64`
compare, then `double_to_big` + bignum compare. Details in `src/compare/numbers.rs`.

### `hashing/` — `phash2`/`phash` (pinned) + the internal structural hash
Split by responsibility: `mod.rs` (docs + the internal structural hash `term_hash` + tests), `common.rs`
(`HCONST*`/`FN*`, `mix32`/`mix3`, `uint32_hash*`/`sint32_hash`/`uint32_hash_step`, `le32`, `block_hash`,
`bitstring_parts_any`, `base_bits`, the work-stack item types — all `pub(super)`), `make_hash2.rs` (the
`make_hash2` port + `compute_record_definition_hash`), `make_hash.rs` (the `make_hash` port +
`hash_binary_bytes`).
- **The pinned contract: `make_hash2`/`make_hash`** — bit-exact u32-wrapping ports of
  `erl_term_hashing.c` (`make_hash2_helper`, `make_hash`), i.e. the values `erlang:phash2/1,2` and
  `erlang:phash/2` return; unlike `term_hash`, **these values ARE part of the Level-1 drop-in contract**
  and are cross-checked against the live oracle.
  - `pub fn make_hash2(t, atoms) -> TermResult<u32>` (`erl_term_hashing.c:1597-1601`): `hash == 0` entry
    fast paths (`atom ⇒ hvalue`, `small ⇒ SINT32_HASH`/`NOT_SSMALL28_HASH`, `nil ⇒ 3468870702`), then the
    body with an empty work stack. Covers smalls, atoms, nil, tuples, byte-batched lists, flat maps and
    HAMT maps (xor pair protocol), bitstrings (byte-aligned `block_hash`; non-byte-aligned bit-copy;
    trailing partial byte `UINT32_HASH_2(bitsize, last >> (8-bitsize), HCONST_15)`), floats (`±0.0`
    normalized to `+0.0`), ± bignums, external refs, external pid/port, native records, and funs. An
    **internal** reference is REFUSED with `TermError::RefIdentityUnavailable` (the C mixes
    `internal_ref_numbers(x)[0]`, an id this representation does not carry).
  - `pub fn make_hash(t, atoms) -> TermResult<u32>` (`erl_term_hashing.c:165-425`). `erlang:phash(T, R)`
    is `1 + (make_hash(T) % R)` for `R < 2^32` and `make_hash(T) + 1` for `R = 2^32` (`bif.c:5413-5452`).
  - `pub fn compute_record_definition_hash(def, atoms) -> TermResult<u32>` — the synthetic
    record-definition hash (see `record.rs`); named `compute_*` so it does not collide with
    `record::record_definition_hash` (the header *reader*) under `lib.rs`'s glob re-exports.
  - **Native records hash bit-exactly.** The record arm of both functions mixes the definition's **real
    computed** hash (`compute_record_definition_hash` over the definition block —
    `module,name,is_exported,keys-in-key-order`, the C's `make_hash2(make_tuple(&defp->hash))`,
    `erl_record.c:1041-1054`) — never the placeholder `record::write_record` stores — and walks the
    stored values in the instance's sorted-key order. Oracle-verified via `tests/record_ref_hash.rs`.
  - **Fun-kind coverage:** `FunKind::External` (`EXPORT_EXT`) and `FunKind::Imported` (`NEW_FUN_EXT`)
    compute the oracle's value. `FunKind::Local` (a `make_fun3` closure) returns
    `TermError::FunIdentityUnavailable` — its inlined 2-word descriptor carries no module and no
    `old_uniq`, so the C's `UINT32_HASH_2(num_free, hvalue(module), HCONST)` is not reproducible.
- **The internal structural hash: `term_hash`.** `pub fn term_hash(a, atoms) -> u64` — deterministic,
  reproducing the *properties* of `make_hash2`: equal terms hash equal. **Its exact values are internal
  and NOT part of any compatibility contract**; the only guarantee is `eq(a,b,atoms)` ⇒ equal hashes.

### `ihash.rs` — the bit-exact `erts_map_hash`
The **erlang map hash** (`erl_term_hashing.c:1955-2130`, `IHASH_*`/`make_internal_hash`):
`erts_map_hash`/`erts_internal_hash`/`erts_internal_salted_hash`/`ihash_mix64`, plus the map-trie helpers
`swizzle_map_hash` (nibble reversal of the 64-bit hash) and `hashmap_nibble` (the level-`L`
least-significant nibble). This is what `map.rs` places leaves with, so a `write_map`-built hash map's
stored pair order equals the oracle's `maps:to_list/1` order (`tests/map_hash.rs`). Classes whose C hash
is pointer-derived (external pid/port/ref, external/local fun, internal ref) use a documented
deterministic substitute; the C hash is itself documented non-portable across VM instances.

### `layout.rs` — canonical per-object layout (single source of truth)
The per-object layout walk every copier/sizer adopts: how many heap words one object occupies and which
payload words are term references a copier must forward. `size.rs` consumes it; `beam_message::layout`
re-exports the walk and `beam_gc::copy::move_size` is `object_words` plus the copy-specific `ErlSubBits`
override. Full per-shape table in `src/layout.rs`.
- `pub unsafe fn object_words(header: Eterm, ptr: *const Eterm) -> usize` — words the boxed object
  occupies, **header included**: tuple/record `arity + 1`; fun `1 + fun_desc_words(kind) + fun_num_free`;
  flatmap `size + 3`; HAMT node `1 + header_arity + popcount(MAP_HEADER_VAL)`; other things
  `thing_arityval + 1`; `ErlSubBits` → `ERL_REFC_BITS_SIZE` (8) when `orig` is a `BinRef`,
  `heap_bits_size_words(end - start)` for a match context over an on-heap bitstring, else
  `ERL_SUB_BITS_SIZE` (5). The shared zero-arity tuple literal (`tuple0()`) is the caller's special case
  (`size_object` skips it; `object_words` reports `1`). Offsets are relative to the header word.
- `pub unsafe fn for_each_sizing_child_slot(header, ptr, visit: impl FnMut(usize))` — the **sizing**
  child offsets; an `ErlSubBits` exposes none.
- `pub unsafe fn for_each_child_slot(header, ptr, visit)` — the **copy** child offsets; identical except
  an `ErlSubBits` exposes its `orig` word (`SUB_BITS_ORIG_WORD` = 4). This sizing/copying divergence is
  deliberate (`copy.c:219-242` vs `copy.c:1677-1700`).
- `bitstring.rs` exports the `SUB_BITS_{BASE,START,END,ORIG}_WORD` indices.

### `size.rs` — object word size
- `pub fn size_object(a: Eterm) -> usize` — the *flat* number of heap words the term graph occupies,
  reproducing `size_object_x(obj, NULL)` (`copy.c`): `0` for immediates, `2` per cons cell, `arity + 1`
  per boxed object, plus nested sizes. Total and **bounded**: a cyclic or forged graph returns a defined
  partial count instead of looping.
- `pub fn size_object_bounded(a: Eterm, limit: usize) -> Result<usize, TermError>` — `Ok(n)` iff the
  graph is **acyclic** and its flat size is `n <= limit`; `Err(TermGraphTooLarge)` for an acyclic graph
  over `limit`; `Err(TermGraphCyclic)` for a cycle (takes priority over the size limit). Both share ONE
  iterative walk; cycle detection is **path-based** (`on_path`), so a shared-but-acyclic subterm is
  counted once per occurrence. A non-term word contributes `0` and is never dereferenced. Per-object
  interior comes from `layout` (`object_words` + `for_each_sizing_child_slot`), so the crate holds
  exactly one definition of an object's interior.

### `thing.rs` — off-heap term materialization (heap-agnostic)
Frozen-layout **writers and readers for the boxed "thing" families this crate owns** — float, bignum,
flat map, external pid/port/ref (`ExternalThing`) blocks. Every writer is **heap-agnostic**: the caller
allocates the words (via `beam_alloc` in production, `heap::Heap` in tests) and passes `hp: *mut Eterm`;
the writer fills the block and returns the tagged root `Eterm`. Word counts/bit patterns are
byte-identical to the oracle-verified `beam_codec::term`, which forwards here. Readers are `unsafe` with
a caller invariant ("`t` is a live thing of this kind") and return borrowed slices. Full per-block
layout in `src/thing.rs`.
- Float: `pub const FLOAT_SIZE_WORDS: usize = 2`; `unsafe write_float(hp, value: f64) -> Eterm`
  (`[HEADER_FLONUM, bits]`, root `make_boxed(hp)`); `unsafe float_value(t) -> f64`; `unsafe float_bits(t)
  -> u64` (re-exports of the single public definitions in `bitstring.rs`).
- Bignum: `pub const fn big_size_words(n_digits) -> usize` (`n_digits + 1`, `BIG_NEED_SIZE`); `unsafe
  write_big(hp, negative: bool, digits: &[u64]) -> Eterm` (header arity = digit count; one `ErtsDigit` =
  `u64` per word, least-significant-first, sign in `_BIG_SIGN_BIT`; `digits` non-empty); `unsafe
  big_parts<'a>(t) -> (bool, &'a [u64])`.
- Flat map: `pub const fn flatmap_size_words(n_pairs) -> usize` (`3` empty, else `4 + 2n`); `unsafe
  write_flatmap(hp, pairs: &[(Eterm, Eterm)]) -> Eterm` (the C block `[arityval(n), k0..k(n-1),
  MAP_HEADER_FLATMAP, n, keys-tuple, v0..v(n-1)]`, root `make_boxed(hp + 1 + n)`; `n_pairs ≤
  MAP_SMALL_MAP_LIMIT`); `unsafe flatmap_parts<'a>(t) -> (&'a [Eterm], &'a [Eterm])`.
- External things (C `ExternalThing`, incl. the M1 "`next` slot holds `creation`" substitution): `pub
  const MAX_REF_IDS: usize = 5` (`ERTS_MAX_REF_NUMBERS`); `external_pid_size_words()` (4),
  `external_port_size_words()` (4), `external_ref_size_words(n_ids)` (`3 + n_ids/2 + 1` on `ARCH_64`);
  the writers `unsafe write_external_pid(hp, node, number: u32, serial: u32, creation: u32)`, `unsafe
  write_external_port(hp, node, number: u64, creation: u32)`, `unsafe write_external_ref(hp, node, ids:
  &[u32], creation: u32)` and their `unsafe *_parts` inverses (`external_pid_parts`/`external_port_parts`/
  `external_ref_parts`). **`ids` may be empty** (`0 <= n <= MAX_REF_IDS`): the oracle accepts a zero-id
  external reference (`binary_to_term(<<131,90,0:16,Node,0:32>>)` round-trips), block `[header, node,
  creation, count = 0]` — `external_ref_size_words(0) == 4` — `external_ref_parts` reports an empty
  slice, and hashing mixes the zeroed word after the count ⇒ `0`.

### `bitstring.rs` — bit-level payloads
The `ErlHeapBits` / `ErlSubBits` block writers and readers plus the crate's public bit/float payload
helpers (ground truth `erl_bits.h:60-176`, `erl_bits.c:490-519`). Bits are **MSB-first inside each
byte** (so `<<1:3>>` is the byte `0b0010_0000`); the unused trailing bits of the last byte are zeroed.
Full layout in `src/bitstring.rs`.
- `pub const fn bit_count_to_bytes(n_bits) -> usize` (`NBYTES`, `ceil(n_bits/8)`, overflow-free); `pub
  const fn heap_bits_size_words(n_bits) -> usize` (`2 + ceil(ceil(n_bits/8)/8)`, `heap_bits_size`); `pub
  const ERL_SUB_BITS_FLAG_MASK = 0x3`, `ERL_SUB_BITS_FLAG_MUTABLE`/`ERL_SUB_BITS_FLAG_VOLATILE`,
  `BITSTRING_POINTER_FLAG_MASK` (alias of the flag mask).
- `unsafe write_heap_bits(hp, bytes: &[u8], n_bits) -> TermResult<Eterm>` — the `ErlHeapBits` writer
  (`[make_header(words-1, HEAP_BITS_SUBTAG), n_bits, data…]`, root `make_boxed(hp)`); a `bytes` shorter
  than `ceil(n_bits/8)` yields `TermError::BadBitstring`.
- `unsafe write_binary(hp, bytes: &[u8]) -> TermResult<Eterm>` — a bitstring of `bytes.len()*8` bits.
- `unsafe write_sub_bits(hp, base_flags, start, end, orig) -> Eterm` / `unsafe sub_bits_parts(t) ->
  (Eterm, Eterm, Eterm, Eterm)` — the 5-word `ErlSubBits` block.
- `unsafe bitstring_parts(t) -> (*const u8, usize, usize)` (data base, **bit** offset, **bit** size);
  `unsafe bit_at(t, i) -> bool`.
- `unsafe float_bits(t) -> u64` / `unsafe float_value(t) -> f64` — the crate's single public float
  payload readers (`compare`/`hashing`/`thing` forward/re-export them).

### `refc.rs` — off-heap (`refc`) binaries
The on-heap `BinRef` + `ErlSubBits` **pair** for a binary/bitstring larger than the on-heap limits
(ground truth `erl_bits.h:135-176`, `erl_bits.c:2672-2712`). The 8-word arrangement is `[BinRef(3)]
[ErlSubBits(5)]` with the tagged root at `hp + 3`. `beam_term` cannot allocate the off-heap payload, so
every writer is heap-agnostic over a caller `hp`/`val` (`src/refc.rs`).
- `pub const fn bin_ref_size_words() -> usize` (3), `pub const fn sub_bits_size_words() -> usize` (5),
  `pub const ERL_REFC_BITS_SIZE = 8`, `pub const ERL_ONHEAP_BINARY_LIMIT = 64`, `pub const
  ERL_ONHEAP_BITS_LIMIT = 512`.
- `unsafe write_bin_ref(hp, val, next) -> Eterm`; `unsafe bin_ref_parts(t) -> (*mut Eterm, *mut Eterm)`.
- `unsafe write_refc_binary(hp, val, next, base, offset_bits, size_bits) -> Eterm` — the full pair.
- `unsafe is_refc_binary(t) -> bool`, `unsafe refc_offset(t) -> usize` (**bits**), `unsafe refc_size(t)
  -> usize` (**bits**, `end - start`), `unsafe refc_bytes<'a>(t) -> &'a [u8]` (`ceil((offset+size)/8) -
  offset/8` bytes).
- **Frozen inter-crate contract (`beam_alloc`/`beam_gc`):** `BinRef.val` holds a raw pointer to the
  payload's **first data byte** (the off-heap object header is `beam_alloc`'s concern); `ErlSubBits.orig`
  is a term (a boxed `BinRef`, or an on-heap bitstring for a match context); the 8 words are moved/copied
  together with `orig` fixed up to the new `BinRef` address.

### `map.rs` — flat map + HAMT/CHAMP builder, lookups, structural size
The crate's second frozen interior: the Erlang **map**. A map is a boxed `MAP_SUBTAG` term in one of two
shapes (ground truth `erl_map.h`, `erl_map.c`): a **flat map** (`MAP_HEADER_FLATMAP`, written by
`thing.rs`) for ≤ `MAP_SMALL_MAP_LIMIT` pairs, else a **hash map** — a 16-way (`4` hash bits/level,
`HAMT_MAX_LEVEL = 16`) compressed hash-array-mapped trie. Trie node layouts (frozen, exactly what
`beam_gc::copy::move_size` sizes): head-array `[hdr][size][16 slots]` (18 words), head-bitmap
`[hdr(bmp)][size][n slots]` (`2+n`), inner bitmap `[hdr(bmp)][n slots]` (`1+n`), collision node = a
`make_arityval(n)` tuple of `n` key/value cons cells. One **term per slot**: a leaf cons cell, a pointer
to a sub-node, or a collision tuple. Level `L` selects the **`L`-th least-significant** nibble of the
key's `ihash::erts_map_hash` — `hashmap_nibble(hash, L)` — so level `0` (the root) is the LSB nibble.
- `pub const MAP_SMALL_MAP_LIMIT: usize = 32;` (`erl_map.h:96`); `pub const HAMT_MAX_LEVEL: usize = 16;`.
- `pub const fn make_map_header(map_type, arity, val) -> Eterm` (`MAKE_MAP_HEADER`); `pub const fn
  map_words_capacity(n_pairs) -> usize` (exact for a flat map, sound upper bound for a hash map).
- `unsafe write_map(hp, pairs: &[(Eterm,Eterm)], atoms) -> TermResult<Eterm>` — dedups (last value
  wins), flat map when ≤ limit else a hash map keyed by `ihash::erts_map_hash`; leaves laid out
  ascending by `swizzle_map_hash(hash)` (the C's `hxnodecmp` order) with equal-hash runs broken by `cmp`
  on the key, so the trie stores its pairs in the oracle's `maps:to_list/1` order and the buffer is
  insertion-order free.
- `map_size(t, atoms) -> TermResult<usize>`, `map_get(t, key, atoms) -> TermResult<Option<Eterm>>`,
  `map_pairs(t, atoms) -> TermResult<Vec<(Eterm,Eterm)>>` (sorted by `compare::cmp`),
  `map_pairs_storage_order(t) -> TermResult<Vec<(Eterm,Eterm)>>` (the *stored* pair order — flat map ⇒
  keys-tuple order, hash map ⇒ HAMT slot traversal; the order an image-preserving serializer needs),
  `unsafe map_put(hp, t, key, val, atoms)` / `unsafe map_remove(hp, t, key, atoms)` (functional).
- `pub(crate) map_pairs_flatmap_order(t, atoms)` — pairs in internal flat-map key order (so a flat map
  and a hash map of the same pairs present identically to `compare`/`hashing`).
- Non-boxed / non-map terms yield `TermError::BadMap` (never a deref of an immediate).

### `fun.rs` — the on-heap fun / export term layout
The boxed closure term: a `make_fun3` local closure or an `EXPORT_EXT`/`NEW_FUN_EXT` literal (ground
truth `erl_term.h:403-425`, `erl_fun.h:61-99`, `emu/instrs.tab:545-584`,
`external.c:3966-4003/5006-5040/5102-5176`, `utils.c:2149-2210`). Full layout in `src/fun.rs`.
- **Header word (C-exact):** bits 0..5 `FUN_SUBTAG` (`0x14`), bits 8..15 `arity`, bits 16..23 `num_free`,
  bit 24 `FUN_HEADER_KIND_OFFS` (C external), bit 25 `FUN_HEADER_IMPORTED_OFFS` (this crate's
  `NEW_FUN_EXT` marker).
- Constants: `FUN_HEADER_ARITY_OFFS=8`, `FUN_HEADER_ENV_SIZE_OFFS=16`, `FUN_HEADER_KIND_OFFS=24`,
  `FUN_HEADER_IMPORTED_OFFS=25`, `FUN_MAX_FIELD=0xFF`, `MAX_ARG=255`, `FUN_DESC_WORDS_LOCAL=2`,
  `FUN_DESC_WORDS_EXTERNAL=3`, `FUN_DESC_WORDS_IMPORTED=10`.
- `pub enum FunKind { Local, External, Imported }`; `pub struct FunPid { node: Atom, number, serial,
  creation: u32 }`.
- Pure (no `unsafe`, take a *header word*) helpers: `make_fun_header(arity, num_free, kind) ->
  TermResult<Eterm>`, `fun_kind_of_header(hdr) -> FunKind` (const), `fun_kind(hdr) -> TermResult<FunKind>`,
  `fun_arity`/`fun_num_free` (const), `fun_desc_words(kind)`, `fun_words(kind, num_free)`,
  `fun_desc_words_header`/`fun_env_offset_header`/`fun_block_words_header` (term→error on non-fun);
  `pub fn fun_block_words(t: Eterm) -> TermResult<usize>`.
- **The external fun's true arity is carried out of band.** The header's 8-bit `arity` field is what the
  C's `fun_info(F, arity)` reports (true arity truncated to its low byte), while the C encoder emits the
  `Export`'s full `info.mfa.arity` (`external.c:3997-4001`). So `FUN_DESC_WORDS_EXTERNAL = 3` and the
  descriptor is `[hdr][module][function][true arity]`; `write_fun_external` accepts **any** arity (masks
  the header field with `FUN_MAX_FIELD`, keeping `num_free == 0` so block sizing stays `1 + 3`), and
  `fun_arity` remains the header reader. Oracle-verified: an `EXPORT_EXT` blob with arity 256 reports
  `{arity,0}` from `fun_info/2` yet re-encodes 256.
- Heap-agnostic writers (`pub unsafe fn`, caller reserves `fun_words(kind, num_free)` words):
  `write_fun_local(hp, arity, uniq: u64, index: u32, env: &[Eterm])`, `write_fun_external(hp, arity,
  module: Atom, function: Atom)`, `write_fun_imported(hp, arity, module: Atom, uniq: &[u8;16], index,
  old_index, old_uniq: u32, creator: FunPid, env: &[Eterm])`.
- Accessors (`pub unsafe fn`, wrong-kind/non-fun ⇒ `TermError::BadFun`, never a panic): `fun_true_arity ->
  TermResult<usize>` (the **true** arity — descriptor word 3 for `External`, the header field for
  `Local`/`Imported`; what an encoder/`compare`/hashing must use), `fun_uniq -> TermResult<u64>`,
  `fun_uniq_bytes -> TermResult<[u8;16]>`, `fun_index`/`fun_old_index`/`fun_old_uniq -> TermResult<u32>`,
  `fun_module`/`fun_function -> TermResult<Atom>`, `fun_creator_pid -> TermResult<FunPid>`, `fun_env ->
  TermResult<&[Eterm]>`.
- **Word layout:** `env[i]` at `hp[1 + desc_words(kind) + i]`, block = `1 + desc_words + num_free` words:
  - `Local` = `[hdr][uniq:u64][index:u32][env…]`;
  - `External` = `[hdr][module:Atom][function:Atom][arity:usize raw]`;
  - `Imported` = `[hdr][module][uniq0..8][uniq8..16][index][old_index][old_uniq][pid_node][pid_number]
    [pid_serial][pid_creation][env…]`.
- `size.rs` sizes a fun as `1 + desc_words + num_free`; `compare.rs` orders funs structurally (`Local` by
  `(uniq,index)` then `num_free` then env; `External` by `(module,function,arity)`; `Imported` by
  `(module,index,old_uniq)` then `num_free` then env); `hashing/`'s `term_hash` hashes exactly the fields
  `compare` compares.

### `record.rs` — native records (`ErtsRecordDefinition`/`ErtsRecordInstance`)
The OTP 29 native-record term interior (ground truth `erl_record.h`/`erl_record.c`: `records_create_4`,
`erts_canonical_record_def`, `records_get_2`, `records_get_field_names_1`, `records_is_exported_1`;
`external.c` `RECORD_EXT`). A record is **two** boxed things (full layout in `src/record.rs`):
- **Instance** (`RECORD_INST_SIZE(n) == 2 + n` words), root `make_boxed(hp)`: `[0] make_record_header(n +
  1)`, `[1] record_definition = make_boxed(def)` (a **tuple-tagged** pointer), `[2..2+n]` the field
  **values in sorted-key order**. `record_field_count = header_arity - 1`.
- **Definition** (`RECORD_DEF_SIZE(n) == 6 + n` words; its `thing_word` IS an arityval, so it reads as a
  tuple via `tuple_val`): `[0] make_arityval(n + 5)`, `[1] field_order` (tuple `[make_arityval(n)]
  [rank…]`, `tuple0()` for `n == 0`), `[2] hash`, `[3] module`, `[4] name`, `[5] is_exported` (the atom
  `true`/`false`), `[6..6+n]` the field **keys in ascending atom-index order**.
  `field_order[declared_index]` = the field's rank in the sorted keys/values arrays.
- Construction block (exactly what `records_create_4` / the ETF decoder allocate): `[field_order tuple
  (n+1, absent when n==0)][definition][instance]`.

Public surface: size fns `RECORD_INST_FIXED_WORDS = 2` / `RECORD_DEF_FIXED_WORDS = 6` /
`record_inst_size_words` / `record_def_size_words` / `record_block_words` (with `const _: () = assert!`
pins); the writer `unsafe write_record(hp, module, name, is_exported, fields, definition_hash)` (`fields`
in declaration order; sorts keys by `atom_val`, builds `field_order`, stores values in sorted order;
duplicate/non-atom keys and arity overflow ⇒ `TermError::BadRecord`); and the readers (all `unsafe`,
non-record/malformed ⇒ `BadRecord`): `record_field_count`, `record_definition`, `record_values`,
`record_get(t, key, atoms) -> TermResult<Option<Eterm>>` (linear key scan via `compare::eq`),
`record_is_native(t, module, name)`, `record_name_is(t, name)`, the definition readers
`record_definition_{field_count,module,name,is_exported,hash,keys,field_order}` and
`record_declaration_keys(def)`. `AM_TRUE`/`AM_FALSE` (= `Atom 1`/`0`, the `atom.names` order) are
exported for the `is_exported` word.

### `number.rs` — safe numeric → `f64`
The crate's single, safe numeric-to-`f64` conversion (ground truth `big.c:2412-2529` `big_to_double`;
caller `emu/float_instrs.tab` `i_fconv`).
- `pub fn big_to_f64(x: Eterm) -> Option<f64>` — a faithful `big_to_double`: `bitlen = (limbs-1)*64 +
  msd_bits`, reject `bitlen > 1024`, take the top 54 bits and round to nearest **ties-to-even**, scale by
  `2^exp`, apply the sign. `Some(d)` only for a finite representable value; `None` for a non-bignum or an
  out-of-range magnitude (the C's `< 0`, which `i_fconv` turns into `badarith`). The API `beam_engine`'s
  `Fconv` adopts.
- `pub fn number_as_f64(x: Eterm) -> f64` — total/panic-free: small → value, float → value, bignum →
  `big_to_f64` or `±f64::INFINITY` when unrepresentable, non-number → `NaN`. A **best-effort**
  (nearest-double) conversion, NOT used for exact comparison (the arithmetic mixed compare lives in
  `compare/numbers.rs`).

### `error.rs` — crate error types
- `pub enum TermError` / `pub type TermResult<T>` — this crate's error types, reconciled into
  `beam_common::BeamError` by an owned `From` impl.
- `TermError::TermGraphCyclic` / `TermGraphTooLarge` — from `size_object_bounded` (cyclic vs over-limit);
  both map to `BeamError::BadArg`.
- `TermError::FunIdentityUnavailable` — `make_hash`/`make_hash2` on a `make_fun3` `Local` fun (inlined
  descriptor carries no `module`/`old_uniq`). Maps to `BeamError::BadArg` (never a wrong hash).
- `TermError::RefIdentityUnavailable` — `make_hash`/`make_hash2` on an **internal reference** (the C hash
  mixes `internal_ref_numbers(x)[0]`, an id this representation does not carry). Maps to
  `BeamError::BadArg`. The internal structural `term_hash` is unaffected.
- `TermError::BadRef` — a reference accessor applied to a non-M1-reference thing (`boxed::internal_ref_id`
  on a non-term, another boxed kind, a *C-layout* `ErtsORefThing` block, or a non-small id word). Maps to
  `BeamError::BadArg`. Distinct from `RefIdentityUnavailable` (a well-formed M1 reference with an
  unreproducible C hash) and from `NotATerm` (the word is not a term at all).
- `TermError::NotATerm` — the word is not a term at all (see the non-term policy under Constraints).

## Atom-table snapshots — the O(1) append-only memo witness
`AtomSnapshot` + `AtomTable::{snapshot, appended_since, generation}` let a caller that memoizes an *image*
of the table validate it in **O(1)** instead of scanning it (`src/atom.rs`; measured by
`examples/atom_witness.rs`).
- **The contract.** `AtomTable` is append-only: `intern` is its only mutating path (`seed`/
  `seed_from_atom_names` route through it; no remove/rename/reorder API), so an atom index — and any
  prefix of the table — is stable for the table's lifetime. `snapshot()` reads three scalars (identity,
  generation, length); `appended_since(snapshot)` is `true` **only** when (1) `self` is the **same
  instance** as the snapshotting table (a *clone* is a new instance), and (2) the length delta equals the
  generation delta (each append adds one name and bumps the generation by one, so any non-append mutation
  breaks the equality).
- It follows that every index `< snapshot.len()` still names the same atom, any name added since occupies
  an index `>= snapshot.len()`, and `self.len() >= snapshot.len()`. The check never scans, never
  allocates, and never returns a wrong `true`.
- **The memo protocol — both halves required.** A caller holding `(image, snap)` may reuse `image` as an
  exact index→atom image of `shared` iff `shared.appended_since(snap) && image.len() <= shared.len()`,
  then must extend `image` with `shared`'s names at `image.len()..shared.len()`, re-taking `snap =
  shared.snapshot()` on **every** extension, and must drop the memo when a decode fails part-way.
- **Adoption.** `beam_bifs::binconv`'s per-thread `DECODE_SCRATCH` (`../../beam_bifs/src/binconv.rs:1105`)
  currently validates via `prefix_matches` (`:1089`, an O(table) `name()` scan, ~1.6–1.8 ns/atom, 5438 ns
  at 3009 atoms), which makes the ETF round trip verification-bound; the scratch should become
  `RefCell<Option<(AtomTable, AtomSnapshot)>>` and the validation the O(1) pair above (`prefix_matches`
  then dead). Measured on this host (`examples/atom_witness.rs`, release, loadavg ~31): the witness is
  **~0.5–0.9 ns/op flat from 4 to 30 009 atoms** against 0.09 µs–1.22 ms for the `AtomTable::clone()` it
  replaces, so the 5438 ns verification at n=3009 collapses to ~1 ns. In `beam_rt`,
  `CodeServer::atoms_mut()` (`../../beam_rt/src/codeserver.rs:293`) is `Rc::make_mut`, so interning a
  novel name mints a fresh instance and conservatively invalidates the memo (one O(table) rebuild per
  novel-atom batch); the steady state (every atom known) performs no `make_mut`, so the witness holds. A
  runtime that wants novel atoms free too should hold one stable instance (`Rc<RefCell<AtomTable>>`) — a
  `beam_rt` decision.

## Constraints
- **Bit-exact layout (Level-1 drop-in).** Every constant, mask, shift and encoding must match
  `../../erts/emulator/beam/erl_term.h` exactly; a change here breaks the frozen ABI and every crate.
- **Dependencies:** `beam_common` + `libc` only (this crate is a foundation dependency sink).
  `beam_common` is used for exactly one thing: the `From<TermError> for beam_common::BeamError` impl in
  `error.rs` (owned here so every dependent gets `?`-interop for free); `TermError`/`TermResult` remain
  this crate's error types and must not be redefined per dependent.
- **`unsafe` confinement:** confined to `heap`, `literal`, `list`, `boxed`, `tuple`, `thing`, `fun`,
  `map`, `record`, `compare`, `hashing`, `ihash`, `bitstring`, `refc`, `number` (raw pointer deref). Every
  `unsafe fn` documents a `# Safety` section; every `unsafe` block carries a `// SAFETY:` rationale.
  `unsafe_op_in_unsafe_fn` stays clean.
- **A word that is not a term is never dereferenced.** The C ABI's `THE_NON_VALUE` (release `0`; debug
  `make_header(0, FLOAT_SUBTAG)`) has primary tag `HEADER`, which the C `is_boxed` bit trick *accepts*, so
  a bare `is_boxed`-guarded `*boxed_val(x)` could dereference garbage. The guards are
  `tag::is_term_word(x) = !is_header(x)` and `boxed::is_boxed_term(x) = is_boxed(x) && is_term_word(x)` —
  **every deref/classification site must test one of them first** (never bare `is_boxed`). Policy for a
  non-term: fallible APIs return the typed `TermError::NotATerm` (`make_hash`/`make_hash2`); total APIs
  give a defined answer (`ops::class` ⇒ `TermClass::NonValue`; `size_object` ⇒ `0`; `number_as_f64` ⇒
  `NaN`; `term_hash` ⇒ a tagged raw-word hash). `compare::cmp`/`eq`/`cmp_arith` order a non-term **after
  every valid term** (never `Equal` to one; two non-terms by raw word), so `eq ⇒ equal hash` still holds.
  The `pub unsafe fn *_parts` raw readers and `heap.rs` keep their documented "caller passes a live thing
  of this kind" contract — they are not fed by `THE_NON_VALUE`.
- **Pure tag math is `const fn`** where the body allows (`raw`, `from_raw`, `primary_tag`, tag tests,
  `make_small`, `signed_value`, `make_atom`, `atom_val`, `make_arityval`, `arityval`, …) — hot in the
  engine.
- Files target ~1000 lines; one responsibility per file.

## Design Decisions
- **`Eterm` = `#[repr(transparent)]` newtype over `usize`**, not a raw alias: type safety, methods, a
  documented ABI contract, while remaining layout/pass-identical to `ERL_NIF_TERM` (Level-1 drop-in
  requires this — Part I §A.3).
- **Fun descriptors inline the C `entry` pointer (an M1 substitution).** C decodes `EXPORT_EXT` to an
  *external* fun (entry = the canonical `Export`) and `NEW_FUN_EXT` to a *local* fun (entry = an
  out-of-line `ErlFunEntry`). This tree has no fun/export table, so the entry fields live *inside* the
  term (see the `fun.rs` word layout above). Header bit 24 is the C kind bit; bit 25
  (`FUN_HEADER_IMPORTED_OFFS`, ours, valid only while bit 24 is clear) discriminates the two local kinds,
  so a C reader's `is_external_fun` still sees a local fun. Carrying every wire field keeps decode→encode
  byte-exact and lets `beam_engine`/`beam_rt` call the fun later (External ⇒ module+function+arity;
  Imported ⇒ module+index).
- **Checked + unchecked constructors.** `make_small`/`make_arityval` are unchecked (engine hot path,
  mirroring C), but `try_*` variants return `TermResult` at trusted boundaries (arithmetic BIFs, loader,
  `beam_alloc`) so range violations become errors, not silent corruption.
- **`Atom` is `u32`.** The representation allows 58-bit atom indices; the practical table is capped far
  below 2^32 (default `+t` limit 1,048,576), so 32 bits is compact and sufficient. Atoms are only produced
  via `make_atom(Atom)`, so the encoding invariant always holds.
- **`AtomTable` assigns indices in insertion order (0-based).** Reproduces `atom.names` exactly (verified
  against `../../erts/emulator/utils/make_tables`: `#define am_<alias> make_atom($i)` with `$i` the
  0-based file order), required for preloaded modules that reference atoms positionally.
- **`AtomTable::clone` mints a new *identity*, and the O(1) witness depends on it.** Because the table is
  append-only, a monotonic generation plus a length check proves "same table, only appended to" — *given*
  a way to tell two tables apart; a per-construction identity from a process-global counter supplies that.
  Preserving identity across `clone` would be unsound under copy-on-write (`Rc::make_mut` clones the live
  shared table on intern), so a clone deliberately takes a fresh one; the only cost is a conservative memo
  rebuild right after a clone. The counter is bookkeeping: not observable behaviour, not part of any
  compatibility contract.
- **`heap::Heap` is a minimal term-construction arena, NOT the production allocator.** The production
  per-process heap / bump allocator is `beam_alloc`. This module exists only so the representation has a
  concrete, testable backing store for constructing boxed/list terms; `beam_alloc`/`beam_gc` build the
  real thing. It never reallocates (fixed capacity) so interior pointers never dangle.
- **`TUPLE0` is `tuple0()`, a function, not a `const`.** A shared empty-tuple literal must carry
  `TAG_LITERAL_PTR`, which requires forming a pointer to a static block — and Rust rejects
  pointer→integer casts during const evaluation. Same root cause: `make_boxed`/`make_list`/`make_tuple`
  are plain `fn`, not `const fn`.
- **Off-heap term materialization lives here (`thing.rs`), heap-agnostic over a caller `*mut Eterm`.** The
  interior layout of a float/bignum/flatmap/external thing is owned by this crate (the frozen-layout LCA
  of every term-producing crate), so its constructors/accessors belong here rather than duplicated in
  `beam_codec`. This crate **cannot depend on `beam_alloc`** (dependency sink; `beam_alloc` already
  depends on `beam_term`, so the reverse would cycle), which is why every writer takes a caller-allocated
  `hp` and returns the tagged root. The codec keeps only error-wrapping + allocation and forwards the
  word layout here.
- **The bit/float payload readers live in `bitstring.rs`.** `bitstring_parts`, `bit_at`, `float_bits`,
  `float_value` are defined once, publicly, there; `compare.rs` and `hashing/` forward to them and
  `thing.rs` re-exports `float_bits`/`float_value`. One definition avoids the `ambiguous_glob_reexports`
  hazard of two modules exporting the same name.
- **The per-object layout walk has a single definition (`src/layout.rs`).** The words-per-object count and
  the child-slot enumeration for every boxed shape — including the three `ErlSubBits` cases — live in one
  place; `size.rs` and the sibling `beam_message`/`beam_gc` copies are consumers. The sizing and copy
  child enumerations are deliberately split because they diverge for `ErlSubBits`: the sizing walk must
  NOT descend into `orig` (it would double-count the block it already counted), while a copier MUST
  forward it.
- **`cmp_arith`'s mixed integer/float compare is exact (`src/compare/numbers.rs`)** — it ports the C
  `mixed_types` block instead of rounding the integer to `f64`, so a small int ≥ `2^53` and a bignum
  outside the lossless window compare exactly as the oracle does; `double_to_big` is a private single
  definition there.

## Test Strategy
- Per-module `#[cfg(test)] mod tests` unit tests: every `src/*.rs` suite pins that thing's **raw words**
  (the bitstring/refc/thing/record/map/fun/number layouts above, incl. the literal constants
  `HEADER_BIN_REF = 0xA8` / `HEADER_SUB_BITS = 0x124`, the 8-word `[BinRef][ErlSubBits]` pair rooted at
  `hp + 3`, and the `MAX_REF_IDS` / single-digit / empty-flatmap / negative-bignum boundaries), plus
  write→read round-trips and the typed rejections. `src/layout.rs` cross-checks
  `sum(object_words over the graph) == size_object`; `src/compare/numbers.rs` covers the exact mixed
  numeric compare; `src/number.rs` pins hand-computed doubles (`2^60`, `2^64`, `2^100`, `-(2^70)`),
  ties-to-even and the `bitlen` 1024/1025 boundary; `src/hashing/` carries the structural-hash suite + the
  `make_hash2`/`make_hash` oracle vectors + `common`'s `block_hash` pin.
- `tests/layout.rs` — exact raw patterns vs `erl_term.h` (`make_small(0) == 0xF`, `make_atom(0) == 0xB`,
  `NIL == 0x3B`, `make_arityval(1) == 0x40`, cons tag `0x1`) plus boundary sweeps (small-int
  min/max/±1/0, `arityval` at `MAX_ARITYVAL`, literal-bit masking); `tests/abi.rs` (`size_of`/`align_of` =
  `usize`, `Eterm: Copy`, `repr(transparent)`); `tests/non_term.rs` (the crate-wide **non-term policy**: a
  word that is not a term must never crash or be dereferenced by any safe entry point — public API only,
  skips nothing).
- `tests/atom_witness.rs` — the O(1) witness: a per-thread-armed counting `#[global_allocator]` proves
  `snapshot`/`appended_since` allocate **nothing** (deterministic, host-load independent); a
  length-independence timing bound (4 vs 200 000 atoms) is the tripwire for an accidental O(table) scan;
  clone-divergence / non-diverging-clone / independently-built-identical-table / shrunken-table soundness
  cases run at 50 000 atoms.
- `tests/compare_terms.rs` / `tests/compare_external.rs` — term ordering across every class pair and the
  external-thing / record arms (a copy is `=:=` its original; antisymmetry; malformed-record no-panic),
  the latter with a live-`erl` cross-check.
- `tests/size_bounded.rs` — cycles through cons/tuple/local-fun/HAMT head-bitmap nodes and behind a long
  prefix (cyclic beats over-limit), budget inclusivity, non-terms ⇒ `Ok(0)`, a 100 000-element list (no
  stack overflow), exact flat/HAMT map sizes, per-occurrence counting of a shared subterm.
- `tests/internal_ref.rs` — the **M1 internal-reference layout** pinned **literally** (`[make_header(3,
  REF_SUBTAG), make_small(0), make_small(0), make_small(id)]`, raw header `0xD0`), so a drift in this
  crate *or* in the byte-identical `beam_process`/`beam_bifs`/`beam_rt` producers is caught here rather
  than by a cross-crate dependency; plus the writer→accessor round-trip, the typed errors
  (`SmallIntOutOfRange`; `BadRef` on a non-term / immediate / tuple / C-layout arity-2 block / non-small
  id word), the frozen 4-word size, and `=:=`/ordering of equal-id vs different-id references.
- `tests/record_ref_hash.rs` — offline pins for two native records (`(make_hash2, make_hash,
  definition_hash)`) and, when `erlc`/`erl` are present, a real `-record #r{...}` fixture cross-checking
  `phash2(T,1 bsl 32)`/`phash(T,1 bsl 32)-1` (skips loudly otherwise). The corpus uses only keys defined
  in `atom.names` so the oracle's atom indices agree with `AtomTable::from_atom_names` (the C record hash
  is not portable for keys outside `atom.names`, and a record's value walk follows the definition's
  sorted-key order, not the declaration order).
- Live-`erl` oracle cross-checks (**skip loudly, never fail**, when `erl` is absent; text protocol because
  a dev-dependency on `beam_codec` would cycle): `tests/oracle.rs` (map `size`/`get`/`pairs` vs
  `maps:size`/`maps:get`/`lists:sort(maps:keys/1)` across the flat/hash boundary 31/32/33/40/100 keys and
  for atom keys; `bitstring_parts`/`bit_at` vs `bit_size`/per-bit/byte views; bignum
  `eq`/`cmp`/`cmp_arith`/`size_object` vs `=:=`/`<`/`>`/`erts_debug:flat_size/1` across the 2-/3-limb
  boundary; flat-map stored key order vs `maps:to_list/1`); `tests/map_hash.rs` (HAMT pair-storage order:
  two offline-pinned literal vectors — 60 small-int keys `#{K => K*2}`, 40 binary keys `#{<<N>> => N}` —
  plus 33/40/50/64/100-key maps mixing small-int/binary/atom keys); `tests/number_oracle.rs` (`big_to_f64`
  vs `float/1` compared as IEEE-754 bits, or the atom `overflow`, over a bit-length sweep `1..=1100` +
  randomised limbs + curated rounding boundaries; `ERL_TIMEOUT = 60s`); `tests/compare_oracle.rs`
  (`cmp_arith`/`eq` vs `==`/`=:=`/`<`/`>` across the `>2^53` asymmetry, small/bignum × float both orders,
  special values).
- Exhaustive cross-checks against the real system ERTS belong to `tools/differential` (the compatibility
  gate). Iteration: `../../scripts/cargo.sh test -p beam_term`, `../../scripts/cargo.sh build -p
  beam_term`. Never `cd`.

## Routing Table
- `src/tag.rs` → raw tag constants/masks/shifts, `PrimaryTag`/`BoxedTag`, `primary_tag`, `is_header`, `is_term_word`
- `src/eterm.rs` → the `Eterm` word newtype (`raw`/`from_raw`), ABI contract
- `src/immediate.rs` → small ints, atom immediates, NIL, catch, pid/port immediates
- `src/atom.rs` → `Atom` index type, `AtomTable` (interning, `atom.names` seeding/parser, `hvalue` = the C `slot.bucket.hvalue` hashpjw used by `make_hash`/`make_hash2`, and the **O(1) append-only witness** `generation`/`snapshot`/`AtomSnapshot`/`appended_since`)
- `src/list.rs` → cons-cell construction/access (`make_list`, `car`, `cdr`, `is_list`)
- `src/boxed.rs` → boxed pointers, header words, arityval, boxed sub-type predicates, map headers, **the M1 internal-reference thing** (`write_internal_ref`/`internal_ref_id`/`INTERNAL_REF_*`) and the C-layout reader `internal_ref_numbers`
- `src/tuple.rs` → tuple construction/size/element access (`tuple0`)
- `src/heap.rs` → minimal bump term arena (the representation's allocation interface)
- `src/literal.rs` → `TAG_LITERAL_PTR` tagging (`is_literal`/`make_literal`/`clear_literal`) + the literal-area graph walk (`tag_literal_area` slice API + its `set_literal_tag` raw-pointer shim) + `LiteralArea`
- `src/ops.rs` → `TermClass`/`class`, numeric predicates, `THE_NON_VALUE`
- `src/compare.rs` → Erlang term ordering (`cmp`/`eq`/`cmp_arith`), the `erts_cmp` port: full-class structural arms (incl. external pid/port/ref and native records) + the residual deterministic fallbacks; `src/compare/numbers.rs` → the exact `mixed_types` numeric compare (`cmp_numbers`) + the private `double_to_big`
- `src/hashing/mod.rs` → module root: docs + the internal structural hash `term_hash` (+ tests); re-exports the public ERTS-hash API
- `src/hashing/common.rs` → primitives shared by the two ERTS hash ports (`HCONST*`/`FN*`, `mix32`/`mix3`, `uint32_hash*`/`sint32_hash`/`uint32_hash_step`, `le32`, `block_hash`, `bitstring_parts_any`, `base_bits`, work-stack item types; `pub(super)`)
- `src/hashing/make_hash2.rs` → the **bit-exact** ERTS `make_hash2` (`erlang:phash2/1,2`) + `compute_record_definition_hash` (fetch this value for a native record's `hash` word)
- `src/hashing/make_hash.rs` → the **bit-exact** ERTS `make_hash` (`erlang:phash/2`) + `hash_binary_bytes`
- `src/ihash.rs` → the **bit-exact** `erts_map_hash` / `make_internal_hash` port (`erl_term_hashing.c`): `erts_map_hash`/`erts_internal_hash`/`erts_internal_salted_hash`/`ihash_mix64`, plus the map-trie helpers `swizzle_map_hash` (nibble reversal) and `hashmap_nibble` (level-`L` LSB nibble)
- `src/size.rs` → object word size (`size_object`, total/panic-free) + the bounded, cycle-safe `size_object_bounded`, sharing one `size_object_x`-faithful walk
- `src/layout.rs` → canonical per-object layout: `object_words` (words per object, header included) + the sizing (`for_each_sizing_child_slot`) / copy (`for_each_child_slot`) child-slot enumerators incl. the `SUB_BITS` `orig` divergence; the single source of truth `size.rs` consumes
- `src/thing.rs` → heap-agnostic writers + readers for float/bignum/flatmap/external-pid/port/ref blocks (frozen off-heap layout; the codec forwards here)
- `src/fun.rs` → the on-heap fun/export layout: `FunKind`/`FunPid`, `make_fun_header`/`fun_kind`/`fun_arity`/`fun_num_free`/`fun_words`/`fun_block_words`, the `write_fun_{local,external,imported}` writers and the `fun_*` accessors (`erl_term.h`/`erl_fun.h`/`external.c` ground truth)
- `src/map.rs` → the Erlang map: flat-map/HAMT (`CHAMP`) builder (`write_map`, keyed by `ihash::erts_map_hash`), `map_size`/`map_get`/`map_put`/`map_remove`/`map_pairs`/`map_pairs_storage_order`, `MAP_SMALL_MAP_LIMIT`, `make_map_header`/`map_words_capacity` (frozen map node layout)
- `src/record.rs` → native records (`ErtsRecordDefinition`/`ErtsRecordInstance`): size fns, `write_record`, the `record_*` / `record_definition_*` readers (frozen record block; `erl_record.h`/`erl_record.c`)
- `src/number.rs` → safe numeric → `f64`: `big_to_f64` (faithful `big_to_double`) + `number_as_f64`
- `src/bitstring.rs` → bit-level payloads: `ErlHeapBits`/`ErlSubBits` writers + readers, `bit_count_to_bytes`/`heap_bits_size_words`, `bitstring_parts`/`bit_at`, the public `float_bits`/`float_value`
- `src/refc.rs` → off-heap (`refc`) binaries: the `BinRef` + `ErlSubBits` on-heap pair writers/accessors, `ERL_REFC_BITS_SIZE`/`ERL_ONHEAP_*` limits, `is_refc_binary`/`refc_*`
- `src/error.rs` → local `TermError`/`TermResult` (incl. `BadRef`, `FunIdentityUnavailable`, `RefIdentityUnavailable`, `TermGraphCyclic`/`TermGraphTooLarge`, `NotATerm`) + the `From<TermError> for beam_common::BeamError` interop impl (upstream reconciliation so dependents get `?`-interop)
- `tests/` → crate-level integration tests (`layout.rs`, `abi.rs`, `non_term.rs`, `compare_terms.rs`, `compare_external.rs`, `size_bounded.rs`, `internal_ref.rs`, `atom_witness.rs`, `record_ref_hash.rs`, and the live-`erl` oracle cross-checks `oracle.rs`, `map_hash.rs`, `number_oracle.rs`, `compare_oracle.rs`)
- `examples/atom_witness.rs` → micro-benchmark for the witness API (ns/op for `snapshot`/`generation`/`appended_since` hit and miss vs `AtomTable::clone()` across table sizes; `run --release --example atom_witness`)

## Handoffs (owned by other nodes — escalate writes to the parent)
- **The M1 internal-reference seam — the API the next consumer codes against.** A **local-node**
  reference decoded from the wire (`NEWER_REFERENCE_EXT` / `NEW_REFERENCE_EXT`) is materialized by
  reserving `INTERNAL_REF_WORDS` words and calling `boxed::write_internal_ref(hp, id)` — never by
  hand-writing the four words — and an internal reference's id is read back for the encoder with
  `boxed::internal_ref_id(t)` (never `internal_ref_numbers`, which describes the C layout only and would
  return small-int tags for an M1 block). The wire form carries the local node's identity, which the M1
  block does not store (`node`/`serial` are the zeroed terms); a reference for **another** node is an
  external reference and stays on `thing::write_external_ref`. **Parity requirement:** the four words this
  writer emits are byte-identical to `beam_process`'s `MonitorRef::to_eterm`, `beam_bifs`'s
  `make_ref_thing` and `beam_rt`'s native process floor; `tests/internal_ref.rs` pins that shape
  literally. Owner of the consumer: `beam_codec` (outside this node).
- **`beam_codec` can carry an external fun arity > 255 and a zero-length external reference.**
  `fun::write_fun_external` accepts **any** arity (masks the header field with `FUN_MAX_FIELD`; a block for
  an `External` fun with `num_free == 0` is `fun_words(External, 0) == 4` words, the true arity living in
  descriptor word 3 read by `fun::fun_true_arity`), so an encoder should emit `EXPORT_EXT`'s
  `info.mfa.arity` via `fun_true_arity` and reserve `fun_words(External, 0)`. `thing::write_external_ref`
  accepts an **empty** `ids` slice (`external_ref_size_words(0) == 4`). Owner: `beam_codec`.
- **Record / number follow-ups for sibling nodes.** `beam_bifs` (`is_record/2,3`, `records:*`),
  `beam_codec` (`RECORD_EXT`, tag 67) and `beam_engine` (`Fconv` bignum source) can consume `record::*`,
  `number::big_to_f64` and `hashing::compute_record_definition_hash`; the last lets
  `records:create/4`/`update/4` compute the definition's `hash` word (the C's
  `make_hash2(TUPLE(module,name,is_exported,keys-in-declaration-order))`, `erl_record.c:1041-1046`)
  instead of the `make_small(0)` placeholder they currently cache — `write_record` still takes that word as
  a caller-supplied argument. `compare`, `hashing` and `size` all treat native records structurally.
- **`beam_message` may delegate its bounded walk to `size_object_bounded`.** `beam_message/src/copy.rs`
  re-implements a bounded, cycle-safe sizing walk (`bounded_words`, `is_acyclic`, `preflight_words`) to
  guard the messaging copy boundary; `size::size_object_bounded` provides exactly that machinery, so the
  duplicate can collapse onto it. Owner: `beam_message`.

## Known Issues
- **Comparison gaps (`src/compare.rs`).** `cmp`/`eq`/`cmp_arith` faithfully reproduce `erts_cmp_compound`
  for atoms (by name), numbers (small/big/float), tuples, lists, bitstrings, maps (flat *and* hash — size
  first, then the `(key,value)` sequence in flat-map key order) and internal pids/ports; and structurally,
  field by field, external pids/ports/references and native records (so a copy of such a term equals its
  original). Funs are ordered structurally (`cmp_funs`): `Local` by `(uniq, index)` then `num_free` then
  env; `External` by `(module, function, arity)` and after any local kind; `Imported` by `(module, index,
  old_uniq)` then `num_free` then env; a mixed `Local`/`Imported` pair leads with a kind rank (because the
  `Local` descriptor carries no module — a documented deviation from the C). These classes still use a
  **deterministic, panic-free fallback** rather than a structural comparison and must be extended if exact
  ordering is needed: on-heap bin refs (class rank, then object arity, then raw term word); internal
  references (raw payload words — for the M1 block those words *are* the identity, `node`/`serial`/`id`, so
  two refs with the same id are `=:=` and ids order numerically; for a C-layout block it is the packed
  `u32`s); atoms whose name is absent from the passed `AtomTable` (raw atom index). Hash-map comparison
  cannot be byte-matched to ERTS (ERTS's own HAMT order depends on its hash) — the flat-map-key-order rule
  is used for both shapes.
- **`Atom` identity is table-relative, not global (`src/atom.rs` + `src/compare.rs`).** `Atom` is a bare
  index into one `AtomTable`; two atoms carry the same `Eterm` only if their table interned the names in
  the same order. The `cmp_impl` fast path (`a.raw() == b.raw()` ⇒ `Equal`) means two atoms with the same
  index but *different* names (from two tables) compare `Eq`; an atom whose index is out of range of the
  passed table falls back to comparing raw indices. Comparison is name-faithful only when the operands and
  the passed `AtomTable` share one table — a per-module `AtomTable` (as `beam_loader::Module` builds)
  therefore makes cross-module atom comparisons unreliable. The fallback is the `_` arm of `cmp_atoms`
  (`src/compare.rs:361`); it silently breaks **ordering** only — equality (`is_eq`) stays faithful, because
  two distinct atoms always carry distinct indices in one consistent table.
- **`term_hash` is internal, not compatibility-pinned (`src/hashing/mod.rs`).** It guarantees only
  `eq(a,b,atoms) ⇒ term_hash(a,atoms) == term_hash(b,atoms)`; exact bit values are not part of any
  contract. Maps hash structurally (their sorted pairs); funs hash over exactly the fields `compare`
  compares (so `eq ⇒ equal hash`, with the same `External`-last / kind-rank rules); native records hash
  over exactly the fields `cmp_records` compares (the definition address is never mixed, so a record
  equals its copy hash-wise and `beam_ets`'s CHAMP invariant holds). External pid/port carry no pointer in
  their words, so their raw-word hash is content-stable; internal refs and on-heap bin refs use an opaque
  raw-word fallback, so their hash is not stable across processes/images. One marginal `eq ⇒ equal hash`
  gap: `hash_words` mixes the external-ref block's header/arity, but `cmp_external_refs` trims the longer
  id slice's leading **zero** ids, so two external refs of different id-array lengths whose extra ids are
  zero compare `Equal` yet hash differently. Reachable only with a forged/foreign ref, so not observed
  through the safe API.
- **The HAMT is `erts_map_hash`-driven (`src/map.rs` + `src/ihash.rs`).** The map trie is placed by the
  **bit-exact** `erts_map_hash` port, so a `write_map`-built hash map's stored pair order equals the
  oracle's `maps:to_list/1` order (verified by `tests/map_hash.rs`). `swizzle_map_hash` is a nibble
  reversal; level `L` consumes the `L`-th **least-significant** nibble. Atom keys are placed correctly only
  when the `AtomTable` assigns the same indices as the oracle (seeding from `atom.names` gives index
  parity). The C hash is itself documented as non-portable across VM instances ("Limitation #1", because
  it mixes pointers); the pointer-derived classes (external pid/port/ref, external/local fun, internal ref)
  use a documented deterministic substitute (see `src/ihash.rs` module docs).

## Notes for Agents
- **Read `../../erts/emulator/beam/erl_term.h` before changing any constant** — it is the ground truth;
  cite it in comments where a value is non-obvious.
- **The `TAG_LITERAL_PTR` contract.** `TAG_LITERAL_PTR` (bit `0x4`) sits *above* the 2-bit primary tag and
  *inside* the 3-bit `TAG_PTR_MASK` (0x7), so `boxed_val`/`list_val` resolution is unchanged by masking it
  off. The C enables it on 64-bit when there is no OS physical-memory reservation (the common case) or
  under DEBUG; we always define it (Level-1 fidelity). `is_literal` is a bare bit test for `0x4` and is
  meaningful **only** on BOXED/LIST words (an IMMED1 word may have that bit set for unrelated reasons).
- `is_list(NIL)` is **false** — NIL is an `IMMED2` immediate; a "list that is not nil" is exactly a cons
  cell (LIST-tagged word). `List` terms are single words (must stay so for the NIF/ETF/msg ABI).
- `make_arityval` encodes `size << 6` with subtag `0x00`, so an arity of 0 encodes to the raw word `0`;
  table cells/`[Eterm; N]` register files must therefore not treat `0` as "empty" without care.
- **No fully safe dereferencing accessor layer.** Every accessor that reads a boxed pointer's header word
  is `pub unsafe fn` — `car`/`cdr`, `is_tuple`/`tuple_size`/`tuple_element`,
  `is_map`/`is_flatmap`/`is_hashmap`/`is_any_fun`, `is_float`/`float_value`/`float_bits`,
  `is_bitstring`/`is_binary`/`bitstring_size`, `class`/`is_integer`/`is_number`, and the `thing.rs`
  `*_parts` readers. Safe `fn`/`const fn` cover only raw-bit inspection (primary tag, immediates,
  `is_list`/`is_nil`, `is_boxed`/`is_header`, the `*_header` subtag tests,
  `header_subtag`/`map_header_*`/`arityval`, `boxed_val`/`list_val`/`tuple_val` pointer extraction) plus
  `cmp`/`eq`/`term_hash`/`size_object`/`AtomTable` access and the safe map readers
  `map::{map_size,map_get,map_pairs,map_pairs_storage_order}`. A `#![deny(unsafe_code)]` consumer must
  deref `*boxed_val(t)` itself (unsafe) to reach into a tuple/list/map/float/binary; the header-word
  subtag tests (`header_subtag`, `BoxedTag::from_raw`, `is_*_header`) are safe but take a *header word*,
  not a boxed term. The public map/bitstring/refc/record APIs added for siblings
  (`map::{write_map,map_put,map_remove}`, `bitstring::{bitstring_parts,bit_at,float_bits,float_value,
  write_heap_bits,...}`, `refc::{bin_ref_parts,refc_bytes,...}`,
  `record::{write_record,record_get,record_is_native,record_definition_*}`) are likewise `unsafe`:
  callers carry a `// SAFETY:` rationale. The exceptions are the safe map readers above and
  `number::{big_to_f64,number_as_f64}` (a total, validated conversion — the crate's
  `cmp`/`size_object` convention).
- Interior layouts that are NOT externally pinned (list chunking, ETS tree shape) are open and documented
  at their own nodes. The map (flat-map + HAMT node) layout *is* owned here (`src/map.rs`) because
  `beam_gc` sizes it — keep it byte-compatible with the copier.

## See Also
- `../../CONTEXT.md` — workspace contracts (§2 frozen term layout, §3 error strategy).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.1–A.3, §B; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A, §B.
- `../../erts/emulator/beam/CONTEXT.md` — legacy core; its "Term Encoding (Eterm) Reference" section.
- `../beam_abi/` — NIF/driver C ABI; transmutes `Eterm` ↔ `ERL_NIF_TERM`.
- `../beam_alloc/`, `../beam_gc/` — production heap/allocator + collector built on `heap.rs`.
