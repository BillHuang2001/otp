# `beam_term` — Frozen BEAM Term Representation

## Intent
Owns the **frozen, bit-exact tagged term representation** of the Rust ERTS rewrite — the workspace's
most load-bearing shared contract (workspace `CONTEXT.md` §2) — at **Level-1 drop-in fidelity** (a Rust
core and the retained C periphery must not differ by one bit). Ground truth: the legacy C
(`../../erts/emulator/beam/erl_term.h`, `erl_term.c`). Every other crate depends downward on this one for
term construction, inspection and classification; **no other crate may assume a different layout.**
Implements Part I §A.1–A.2 and Part II §B.

## API Surface (FROZEN — every downstream crate routes here)

`Eterm` is a `#[repr(transparent)]` newtype over `usize`, ABI-identical to `ERL_NIF_TERM` (both one
machine word); `beam_abi` transmutes between them. No other crate may redefine it.

### crate root (`src/lib.rs`)
`pub mod {tag, eterm, immediate, atom, list, boxed, tuple, heap, literal, ops, compare, hashing, size,
layout, thing, bitstring, refc, map, fun, record, number, error}` + `pub mod prelude` + flat `pub use`
re-exports of every public item (`use beam_term::prelude::*;`).

### `eterm.rs`
- `pub struct Eterm(usize)` — `#[repr(transparent)]`; `Copy + Clone + PartialEq + Eq + Hash + Debug`.
- `pub const fn Eterm::raw(self) -> usize` / `Eterm::from_raw(raw: usize) -> Eterm`; `Default` → `NIL`.
- `pub const fn Eterm::nil() -> Eterm` / `Eterm::non_value() -> Eterm`.

### `tag.rs`
- Primary: `TAG_PRIMARY_MASK=0x3`, `TAG_PRIMARY_HEADER=0x0`, `TAG_PRIMARY_LIST=0x1`, `TAG_PRIMARY_BOXED=0x2`, `TAG_PRIMARY_IMMED1=0x3`.
- Pointer: `TAG_PTR_MASK=0x7` (64-bit), `TAG_LITERAL_PTR=0x4`.
- IMMED1 (mask `0xF`): `TAG_IMMED1_SIZE=4`, `TAG_IMMED1_PID=0x3`, `TAG_IMMED1_PORT=0x7`, `TAG_IMMED1_IMMED2=0xB`, `TAG_IMMED1_SMALL=0xF`.
- IMMED2 (mask `0x3F`): `TAG_IMMED2_SIZE=6`, `TAG_IMMED2_ATOM=0xB`, `TAG_IMMED2_CATCH=0x1B`, `TAG_IMMED2_NIL=0x3B`.
- Headers: `TAG_HEADER_MASK=0x3F`, `HEADER_ARITY_OFFS=6`, `HEADER_SUBTAG_MASK=0x3C`, `MAX_ARITYVAL=(1<<24)-1`.
- `pub enum PrimaryTag { Header, List, Boxed, Immed1 }`.
- `pub enum BoxedTag { ArityVal=0x00, PosBig=0x08, NegBig=0x0C, Ref=0x10, Fun=0x14, Float=0x18, Record=0x1C, HeapBits=0x20, SubBits=0x24, BinRef=0x28, Map=0x2C, ExternalPid=0x30, ExternalPort=0x34, ExternalRef=0x38 }` (+ `from_raw(subtag)`).
- `pub const fn primary_tag(Eterm) -> PrimaryTag`, `is_header(Eterm) -> bool`, `is_term_word(Eterm) -> bool` (= `!is_header`).

### `immediate.rs`
- `SMALL_BITS=60`, `MAX_SMALL=(1<<59)-1`, `MIN_SMALL=-(1<<59)`; `is_immediate`/`is_not_immediate`; `is_small`/`is_not_small`; `make_small(i64)->Eterm` (unchecked, `debug_assert`); `try_make_small(i64)->TermResult<Eterm>`; `unsigned_value(Eterm)->u64`; `signed_value(Eterm)->i64`; `is_byte(Eterm)->bool`.
- `is_atom`/`is_not_atom`; `make_atom(Atom)->Eterm`; `atom_val(Eterm)->Atom`.
- `pub const NIL: Eterm` (= `0x3B`); `is_nil`/`is_not_nil`.
- `make_catch(u64)`, `is_catch`, `catch_val(Eterm)->u64`.
- `is_internal_pid`, `make_pid_data(ser,num)`, `pid_number`, `pid_serial`; `is_internal_port`, `make_port_data(num)`, `port_number`.

### `atom.rs`
- `pub struct Atom(u32)` — `Copy + Clone + PartialEq + Eq + Hash + PartialOrd + Ord + Debug`.
- `ATOM_INDEX_BITS: u32 = 6`; `MAX_ATOM_INDEX: usize = 2^58-1` (representation ceiling); `Atom::MAX` (= `u32::MAX`, practical ceiling).
- `pub struct AtomTable` — **indices assigned in insertion order starting at 0**, reproducing `atom.names` ordering; one index per distinct name.
- `new()`, `intern(&mut self, &str) -> Atom`, `lookup(&self, &str) -> Option<Atom>`, `name(&self, Atom) -> Option<&str>`, `len`, `is_empty`.
- `seed(&mut self, ordered_names: impl IntoIterator<Item=&str>)`; `seed_from_atom_names(&mut self, src: &str) -> TermResult<()>` (parses the `atom.names` grammar: `atom` <name>+, names bare/`'quoted'`/`CName='printname'`, `#` comments, file order); `from_atom_names(src) -> TermResult<AtomTable>` (the parser is a callback tokenizer — `parse_atom_list` hands each printname to a caller closure and returns its line's name count, and the private `for_each_atom_line` owns the line structure — so seeding allocates one `Arc<str>` per name with no throw-away `Vec<String>`).
- `hvalue(&self, a: Atom) -> u32` — the C `atom_tab(a)->slot.bucket.hvalue` (Dragon-Book `hashpjw` over UTF-8 name bytes + the latin1 clutch, `atom.c:82`); the value `make_hash`/`make_hash2` mix for an atom. Out-of-range index falls back to `a.raw()`.
- Generating `am_*`-equivalent `Atom` constants from `atom.names` is a codegen/boot step routed to `beam_rt`.
- **O(1) append-only witness** (contract: [Atom-table snapshots](#atom-table-snapshots--the-o1-append-only-memo-witness)): `generation() -> u64`, `snapshot() -> AtomSnapshot`, `appended_since(AtomSnapshot) -> bool`; `AtomSnapshot` exposes `len()`/`is_empty()`/`generation()`.
- **Backing store:** `{ names: Vec<Arc<str>>, index: HashMap<Arc<str>, Atom>, instance: u64, generation: u64 }` (the two scalars exist only for the witness); derives `Debug`, hand-writes `Default`/`Clone`. A name is stored **once** as a shared `Arc<str>` both containers hold a handle to (`Arc`, not `Rc`, so the table stays `Send + Sync`); `name` is `O(1)`; `intern` allocates **exactly one `Arc<str>`** for a novel name and **nothing** for a known name (no generation bump); `seed`/`seed_from_atom_names` reserve both containers once (size-hint lower bound / whole-source pre-count) instead of re-growing them; `clone()` deep-copies (a fresh `Arc<str>` per entry plus the two `with_capacity` reservations, so Θ(N)) and **mints a fresh `instance`** — never clone a populated table on a hot path.

### `list.rs` (cons cells, `[CAR,CDR]`, `TAG_PRIMARY_LIST`)
- `make_list(*const Eterm)->Eterm`; `list_val(Eterm)->*mut Eterm`; `ptr_val(Eterm)->*mut Eterm` (generic tagged pointer, masks `TAG_PTR_MASK` incl. `TAG_LITERAL_PTR`).
- `is_list`/`is_not_list`; `is_not_nil_list` (= `is_list`).
- `car(Eterm)->Eterm`; `cdr(Eterm)->Eterm`; `CAR_OFFSET=0`, `CDR_OFFSET=1`.

### `boxed.rs`
- `make_boxed(*mut Eterm)->Eterm`; `boxed_val(Eterm)->*mut Eterm`; `is_boxed`/`is_not_boxed`; `is_boxed_term` (boxed *and* a term word).
- `make_header(size, subtag)`, `header_subtag(Eterm)->u32`, `header_arity(Eterm)->usize`, `thing_arityval(Eterm)->usize`.
- Arityval: `make_arityval(usize)`, `make_arityval_zero()`, `make_arityval_unchecked(usize)`, `try_make_arityval(usize)->TermResult<Eterm>`, `arityval(Eterm)->usize`, `is_arity_value`, `is_sane_arity_value`.
- Predicates: `is_map`/`is_map_header`/`is_flatmap`/`is_hashmap`, `is_float`, `is_big`, `is_internal_ref`, `is_any_fun`, `is_record`, `is_bitstring`/`is_binary`, `is_external_pid`/`is_external_port`/`is_external_ref`, `is_external_fun`/`is_imported_fun`/`is_local_fun`.
- **M1 internal-reference thing (the canonical layout):** frozen 4-word `[make_header(3, REF_SUBTAG), node, serial, id]`, `INTERNAL_REF_PAYLOAD_WORDS = 3` / `INTERNAL_REF_WORDS = 4`; written by `unsafe fn write_internal_ref(hp: *mut Eterm, id: u64) -> TermResult<Eterm>` (heap-agnostic; `SmallIntOutOfRange` if `id` is not a 60-bit small) and read by `unsafe fn internal_ref_id(t: Eterm) -> TermResult<u64>` (`BadRef` for a non-term, another boxed kind, a C-layout arity-2 block, or a non-small id word). The only sanctioned producer/consumer pair.
- `unsafe fn internal_ref_numbers<'a>(t) -> &'a [u32]` — the **C-layout reader** (the `ErtsORefThing` `num[]` ids of a 3-word `[make_header(2, REF_SUBTAG), w1, w2]` block); nothing calls it and it must **never** be fed an M1 reference (would return small-int tags, not ids).
- Map header accessors `MAP_HEADER_TYPE/ARITY/VAL(Hdr)`; `make_record_header(size)` = `make_header(size, RECORD_SUBTAG)`.

### `tuple.rs`
- `make_tuple(*mut Eterm)->Eterm`; `tuple_val(Eterm)->*mut Eterm`; `is_tuple`/`is_not_tuple`; `is_tuple_arity(Eterm, usize)->bool`; `tuple_size(Eterm)->usize`; `tuple_element(Eterm, usize)->Eterm`; `try_tuple_element(Eterm, usize)->TermResult<Eterm>`.
- `pub fn tuple0() -> Eterm` — the shared `TAG_LITERAL_PTR`-tagged empty-tuple literal (`LazyLock` over one leaked word). Zero-arity heap tuples are discouraged.

### `heap.rs` (bump arena)
Minimal and `unsafe`-by-design; `beam_alloc`/`beam_gc` extend/replace it. Never reallocates, so pointers stay valid for the arena's lifetime.
- `pub struct Heap`; `new(capacity_words)`; `capacity()`, `used()`, `available()`, `reset()`, `hp()->*mut Eterm`, `hend()->*mut Eterm`.
- `unsafe fn alloc_words(&mut self, n)->TermResult<*mut Eterm>`; `cons(&mut self, car, cdr)->TermResult<Eterm>`; `tuple(&mut self, elems: &[Eterm])->TermResult<Eterm>`; `unsafe fn alloc_tuple(&mut self, n)->TermResult<*mut Eterm>`.

### `literal.rs` (off-heap literal areas, `TAG_LITERAL_PTR`)
- `make_literal(Eterm)->Eterm`; `is_literal(Eterm)->bool` (bare bit test for `0x4`, meaningful **only** on BOXED/LIST words); `clear_literal(Eterm)->Eterm`.
- `unsafe fn set_literal_tag(term: *mut Eterm, hp_start: *mut Eterm, hsz: usize)` — the `erts_set_literal_tag` graph walk (ORs `TAG_LITERAL_PTR` into every BOXED/LIST word + the root); a thin raw-pointer shim over `tag_literal_area`.
- `tag_literal_area(area: &mut [Eterm], roots: &mut [Eterm])` — the **safe slice-based** form: tags every BOXED/LIST word of the contiguous `area` (stepping over HEADER bodies, with the `HEADER_SUB_BITS` `orig` special case; IMMED1 untouched), then each `roots` word that `is_boxed`/`is_list`. Shares the private `tag_words` walk (`src/literal.rs`), which steps over a HEADER body by `thing_arityval` — except a **fun** header, stepped over by `fun_desc_words(fun_kind_of_header(hdr))` (2/3/10) so the inlined raw descriptor words are never tagged, and `HEADER_SUB_BITS` by `thing_arityval - 1` (the `orig` field).
- `pub struct LiteralArea` — bump-allocated frozen block (`start`/`end`/`off_heap`) with `alloc_words`, `contains(ptr)`, `is_literal(ptr)`.

### `ops.rs`
- `pub enum TermClass { Bitstring, List, Nil, Map, Record, Tuple, Pid, ExternalPid, Port, ExternalPort, Fun, Ref, ExternalRef, Atom, Float, Big, Small, BinRef }` (mirrors the C `*_DEF` virtual tags; `NonValue` is the non-term class; order number < atom < ref < fun < port < pid < tuple < record < map < nil < cons < binary).
- `unsafe fn class(Eterm)->TermClass`; `unsafe fn is_integer(Eterm)` (small ‖ big); `unsafe fn is_number(Eterm)` (integer ‖ float).
- `THE_NON_VALUE` (`0` release, `make_header(0, Float)` debug); `is_non_value`, `is_value`, `is_zero_sized`.

### `compare.rs` (`erts_cmp`)
LCA ordering for `beam_ets` ordered_set, `beam_bifs` comparisons and `<`/`>`:
- `pub fn cmp(a, b, atoms: &AtomTable) -> core::cmp::Ordering` — exact (`=:=`) order, `erts_cmp(a,b,exact=1,eq_only=0)`/`erts_cmp_compound` (`utils.c`); ascending `number < atom < ref < fun < port < pid < tuple < record < map < nil < list < bitstring` (plus external variants).
- `pub fn eq(a, b, atoms) -> bool` — `cmp(a,b).is_eq()` (`=:=`).
- `pub fn cmp_arith(a, b, atoms) -> core::cmp::Ordering` — arithmetic (`==`) order, `erts_cmp(a,b,exact=0,eq_only=0)` (differs from `cmp` only for floats and mixed int/float pairs). Mixed pairs use the C's **exact** algorithm (not an `f64` cast): `1 == 1.0`, `-0.0 == +0.0`, `9007199254740993 > 9007199254740992.0`; NaN/±inf deterministic.
- **Same-class structural arms:** external pids, ports, references and native records are compared field-by-field in the C's order, so a **copy is `=:=` its original**: pid `serial → number → node(sysname, creation)`; port `node(sysname, creation) → number`; ref `node → ids` as a bignum with the highest-index zero words trimmed; record `module → name → is_exported(false<true) → field count → keys → values` (records always structural).
- **Maps (`src/compare/maps.rs`):** two flat maps whose keys are non-descending in `cmp_flatmap_keys` order compare straight out of storage (size, then keys, then values) with **no allocation**; any other pair — a hash map, or a flat map some order-preserving producer (`beam_bifs`/`beam_codec`/`beam_engine` `make_flatmap`) built unsorted — uses the collecting `map_pairs_flatmap_order` fallback. Both paths compare the same key-then-value sequence.

### `compare/numbers.rs`
`pub(super) fn cmp_numbers(...)` — the `mixed_types` port of `erts_cmp_compound` (`utils.c:2345-2460`) plus `erts_float_comp` and the private `double_to_big` (exact integral-`f64` → LS-first `u64` limbs, `big.c:2536`): exact numeric comparison for a pair of *different* numeric classes (`Small,Big`/`Big,Small` by sign; small↔float inside the lossless `±(2^53−2)` window else exact integer subtraction; big↔float by magnitude bands, lossless `big_to_f64`, then exact `double_to_big` + bignum). Consumed by `compare.rs`.

### `hashing/` — `phash2`/`phash` (pinned) + internal structural hash
Split: `mod.rs` (docs + internal `term_hash` + tests), `common.rs` (`HCONST*`/`FN*`, `mix32`/`mix3`, `uint32_hash*`/`sint32_hash`/`uint32_hash_step`, `le32`, `block_hash`, `bitstring_parts_any`, `base_bits`, work-stack items — all `pub(super)`), `make_hash2.rs`, `make_hash.rs`.
- **Pinned contract:** `make_hash2`/`make_hash` are bit-exact u32-wrapping ports of `erl_term_hashing.c` (`make_hash2_helper`, `make_hash`) — the values `erlang:phash2/1,2` and `erlang:phash/2` return; unlike `term_hash` **these ARE part of the Level-1 drop-in contract** and are oracle-cross-checked.
  - `pub fn make_hash2(t, atoms) -> TermResult<u32>` (`erl_term_hashing.c:1597-1601`): `hash == 0` fast paths (`atom ⇒ hvalue`, `small ⇒ SINT32_HASH`/`NOT_SSMALL28_HASH`, `nil ⇒ 3468870702`) then the body. Covers smalls, atoms, nil, tuples, byte-batched lists, flat/HAMT maps (xor pair protocol), bitstrings (byte-aligned `block_hash`, non-byte-aligned bit-copy, trailing partial byte `UINT32_HASH_2(bitsize, last >> (8-bitsize), HCONST_15)`), floats (`±0.0` → `+0.0`), ± bignums, external refs, external pid/port, native records, funs. **Internal** references are REFUSED with `TermError::RefIdentityUnavailable`.
  - `pub fn make_hash(t, atoms) -> TermResult<u32>` (`:165-425`); `erlang:phash(T,R) = 1 + (make_hash(T) % R)` for `R < 2^32`, `make_hash(T)+1` for `R = 2^32` (`bif.c:5413-5452`).
  - `pub fn compute_record_definition_hash(def, atoms) -> TermResult<u32>` — synthetic record-definition hash; named `compute_*` to avoid colliding with `record::record_definition_hash` (a header *reader*) under the glob re-exports.
  - **Native records hash bit-exactly:** both functions mix the definition's **real computed** hash (`module,name,is_exported,keys-in-key-order`, the C's `make_hash2(make_tuple(&defp->hash))`, `erl_record.c:1041-1054`), never the placeholder `write_record` stores, and walk the stored values in the instance's sorted-key order (`tests/record_ref_hash.rs`).
  - **Fun coverage:** `FunKind::External` (`EXPORT_EXT`) and `FunKind::Imported` (`NEW_FUN_EXT`) compute the oracle's value; `FunKind::Local` (`make_fun3`) returns `TermError::FunIdentityUnavailable` (its 2-word descriptor carries no module/`old_uniq`, so the C's `UINT32_HASH_2(num_free, hvalue(module), HCONST)` is not reproducible).
- **Internal structural hash:** `pub fn term_hash(a, atoms) -> u64` — deterministic, `eq(a,b,atoms) ⇒ equal hashes`; **its exact values are internal, NOT part of any compatibility contract.**

### `ihash.rs` — bit-exact `erts_map_hash`
The erlang map hash (`erl_term_hashing.c:1955-2130`): `erts_map_hash`/`erts_internal_hash`/`erts_internal_salted_hash`/`ihash_mix64`, plus `swizzle_map_hash` (nibble reversal of the 64-bit hash) and `hashmap_nibble` (the level-`L` least-significant nibble). Drives `map.rs` leaf placement so a `write_map` hash map's stored pair order equals the oracle's `maps:to_list/1` order (`tests/map_hash.rs`). Pointer-derived classes (external pid/port/ref, external/local fun, internal ref) use a documented deterministic substitute; the C hash is itself non-portable across VM instances.

### `layout.rs` — canonical per-object layout (single source of truth)
How many heap words an object occupies and which payload words are term references a copier must forward. `size.rs` consumes it; `beam_message::layout` re-exports the walk and `beam_gc::copy::move_size` is `object_words` plus the copy-specific `ErlSubBits` override. Per-shape table in `src/layout.rs`.
- `pub unsafe fn object_words(header: Eterm, ptr: *const Eterm) -> usize` (header included): tuple/record `arity + 1`; fun `1 + fun_desc_words(kind) + fun_num_free`; flatmap `size + 3`; HAMT node `1 + header_arity + popcount(MAP_HEADER_VAL)`; other things `thing_arityval + 1`; `ErlSubBits` → `ERL_REFC_BITS_SIZE` (8) when `orig` is a `BinRef`, `heap_bits_size_words(end - start)` for a match context over an on-heap bitstring, else `ERL_SUB_BITS_SIZE` (5). `tuple0()` is the caller's special case (`size_object` skips it; `object_words` reports `1`). Offsets are relative to the header word.
- `pub unsafe fn for_each_sizing_child_slot(header, ptr, visit: impl FnMut(usize))` — sizing child offsets; an `ErlSubBits` exposes none.
- `pub unsafe fn for_each_child_slot(header, ptr, visit)` — copy child offsets; identical except an `ErlSubBits` exposes its `orig` word (`SUB_BITS_ORIG_WORD` = 4). Deliberate divergence (`copy.c:219-242` vs `:1677-1700`).
- `bitstring.rs` exports `SUB_BITS_{BASE,START,END,ORIG}_WORD`.

### `size.rs`
- `pub fn size_object(a: Eterm) -> usize` — flat heap words the graph occupies (`size_object_x(obj,NULL)`, `copy.c`): `0` immediates, `2` per cons cell, `arity + 1` per boxed object, plus nested sizes. Total and **bounded** (a cyclic/forged graph gives a defined partial count).
- `pub fn size_object_bounded(a: Eterm, limit: usize) -> Result<usize, TermError>` — `Ok(n)` iff acyclic and flat size `n <= limit`; `Err(TermGraphTooLarge)` over limit; `Err(TermGraphCyclic)` for a cycle (priority over the limit). Shares ONE iterative walk; **path-based** cycle detection (`on_path`), so a shared-but-acyclic subterm counts once per occurrence. A non-term word contributes `0` and is never dereferenced. Interior driven by `layout` (`object_words` + `for_each_sizing_child_slot`) — exactly one definition of an object's interior.

### `thing.rs` — off-heap term materialization (heap-agnostic)
Readers/writers for the boxed "thing" families this crate owns: float, bignum, flat map, external pid/port/ref (`ExternalThing`). Every writer takes a caller-allocated `hp: *mut Eterm` (via `beam_alloc` in production, `heap::Heap` in tests) and returns the tagged root; word counts/bit patterns are byte-identical to the oracle-verified `beam_codec::term`, which forwards here. Readers are `unsafe` with a caller invariant and return borrowed slices. Per-block layout in `src/thing.rs`.
- Float: `FLOAT_SIZE_WORDS: usize = 2`; `unsafe write_float(hp, value: f64)->Eterm` (`[HEADER_FLONUM, bits]`, root `make_boxed(hp)`); `unsafe float_value(t)->f64`; `unsafe float_bits(t)->u64` (re-exports of the `bitstring.rs` definitions).
- Bignum: `pub const fn big_size_words(n_digits)->usize` (`n_digits + 1`, `BIG_NEED_SIZE`); `unsafe write_big(hp, negative: bool, digits: &[u64])->Eterm` (header arity = digit count; one `ErtsDigit`=`u64` per word, LS-first, sign in `_BIG_SIGN_BIT`; `digits` non-empty); `unsafe big_parts<'a>(t)->(bool, &'a [u64])`.
- Flat map: `pub const fn flatmap_size_words(n_pairs)->usize` (`3` empty, else `4 + 2n`); `unsafe write_flatmap(hp, pairs: &[(Eterm,Eterm)])->Eterm` (the C block `[arityval(n), k0..k(n-1), MAP_HEADER_FLATMAP, n, keys-tuple, v0..v(n-1)]`, root `make_boxed(hp + 1 + n)`; `n_pairs ≤ MAP_SMALL_MAP_LIMIT`); `unsafe flatmap_parts<'a>(t)->(&'a [Eterm], &'a [Eterm])`.
- External things (incl. the M1 "`next` holds `creation`" substitution): `MAX_REF_IDS: usize = 5`; `external_pid_size_words()` (4), `external_port_size_words()` (4), `external_ref_size_words(n_ids)` (`3 + n_ids/2 + 1` on `ARCH_64`); `unsafe write_external_pid(hp, node, number: u32, serial: u32, creation: u32)`, `unsafe write_external_port(hp, node, number: u64, creation: u32)`, `unsafe write_external_ref(hp, node, ids: &[u32], creation: u32)`, and the `unsafe *_parts` inverses (`external_pid_parts`/`external_port_parts`/`external_ref_parts`). **`ids` may be empty** (`0 <= n <= MAX_REF_IDS`): a zero-id external reference round-trips (`binary_to_term(<<131,90,0:16,Node,0:32>>)`), block `[header, node, creation, count = 0]`, `external_ref_size_words(0) == 4`, `external_ref_parts` empty, hashing mixes the zeroed word after count ⇒ `0`.

### `bitstring.rs` — bit-level payloads
`ErlHeapBits`/`ErlSubBits` writers+readers plus the public bit/float payload helpers (`erl_bits.h:60-176`). Bits are **MSB-first inside each byte**; the last byte's unused trailing bits are zeroed. Layout in `src/bitstring.rs`.
- `pub const fn bit_count_to_bytes(n_bits)->usize` (`ceil(n_bits/8)`, overflow-free); `pub const fn heap_bits_size_words(n_bits)->usize` (`2 + ceil(ceil(n_bits/8)/8)`); `ERL_SUB_BITS_FLAG_MASK = 0x3`, `ERL_SUB_BITS_FLAG_MUTABLE`/`_VOLATILE`, `BITSTRING_POINTER_FLAG_MASK` (alias).
- `unsafe write_heap_bits(hp, bytes: &[u8], n_bits)->TermResult<Eterm>` (`[make_header(words-1, HEAP_BITS_SUBTAG), n_bits, data…]`, root `make_boxed(hp)`; `BadBitstring` if `bytes` shorter than `ceil(n_bits/8)`).
- `unsafe write_binary(hp, bytes: &[u8])->TermResult<Eterm>`; `unsafe write_sub_bits(hp, base_flags, start, end, orig)->Eterm` / `unsafe sub_bits_parts(t)->(Eterm,Eterm,Eterm,Eterm)` (the 5-word `ErlSubBits` block).
- `unsafe bitstring_parts(t)->(*const u8, usize, usize)` (base, **bit** offset, **bit** size); `unsafe bit_at(t, i)->bool`; `unsafe float_bits(t)->u64` / `float_value(t)->f64` (the crate's single float readers).

### `refc.rs` — off-heap (`refc`) binaries
The on-heap `BinRef` + `ErlSubBits` pair for a binary/bitstring above the on-heap limits (`erl_bits.h:135-176`). The 8-word arrangement is `[BinRef(3)][ErlSubBits(5)]`, tagged root at `hp + 3`; writers are heap-agnostic over a caller `hp`/`val` (`src/refc.rs`).
- `pub const fn bin_ref_size_words()->usize` (3), `pub const fn sub_bits_size_words()->usize` (5), `ERL_REFC_BITS_SIZE = 8`, `ERL_ONHEAP_BINARY_LIMIT = 64`, `ERL_ONHEAP_BITS_LIMIT = 512`.
- `unsafe write_bin_ref(hp, val, next)->Eterm`; `unsafe bin_ref_parts(t)->(*mut Eterm, *mut Eterm)`; `unsafe write_refc_binary(hp, val, next, base, offset_bits, size_bits)->Eterm` (the full pair).
- `unsafe is_refc_binary(t)->bool`, `unsafe refc_offset(t)->usize` (**bits**), `unsafe refc_size(t)->usize` (**bits**, `end - start`), `unsafe refc_bytes<'a>(t)->&'a [u8]`.
- **Frozen inter-crate contract (`beam_alloc`/`beam_gc`):** `BinRef.val` = raw pointer to the payload's **first data byte** (the off-heap header is `beam_alloc`'s concern); `ErlSubBits.orig` is a term (a boxed `BinRef`, or an on-heap bitstring for a match context); the 8 words move/copy together with `orig` fixed up to the new `BinRef`.

### `map.rs` — flat map + HAMT/CHAMP builder, lookups, size
The Erlang map: a boxed `MAP_SUBTAG` term, flat (`MAP_HEADER_FLATMAP`, written by `thing.rs`) for ≤ `MAP_SMALL_MAP_LIMIT` pairs else a **hash map** — a 16-way (`4` hash bits/level, `HAMT_MAX_LEVEL = 16`) CHAMP. Trie node layouts (frozen; what `beam_gc::copy::move_size` sizes): head-array `[hdr][size][16 slots]` (18 words), head-bitmap `[hdr(bmp)][size][n slots]` (`2+n`), inner bitmap `[hdr(bmp)][n slots]` (`1+n`), collision node = a `make_arityval(n)` tuple of `n` key/value cons cells. One **term per slot** (leaf cons cell, sub-node pointer, or collision tuple). Level `L` selects the **`L`-th least-significant** nibble of the key's `ihash::erts_map_hash` via `hashmap_nibble` (level 0 = LSB nibble). Ground truth `erl_map.h`/`erl_map.c`.
- `MAP_SMALL_MAP_LIMIT: usize = 32` (`erl_map.h:96`); `HAMT_MAX_LEVEL: usize = 16`.
- `pub const fn make_map_header(map_type, arity, val)->Eterm`; `pub const fn map_words_capacity(n_pairs)->usize` (exact for flat, sound upper bound for hash).
- `unsafe write_map(hp, pairs: &[(Eterm,Eterm)], atoms)->TermResult<Eterm>` — dedups (last wins), flat ≤ limit else hash keyed by `erts_map_hash`; leaves ascending by `swizzle_map_hash(hash)` (the C's `hxnodecmp` order) with equal-hash runs broken by `cmp` on the key, so the trie stores pairs in the oracle's `maps:to_list/1` order (insertion-order free).
- `map_size(t, atoms)->TermResult<usize>`, `map_get(t, key, atoms)->TermResult<Option<Eterm>>`, `map_pairs(t, atoms)->TermResult<Vec<(Eterm,Eterm)>>` (sorted by `compare::cmp`), `map_pairs_storage_order(t)->TermResult<Vec<(Eterm,Eterm)>>` (the *stored* order: flat ⇒ keys-tuple, hash ⇒ HAMT traversal; the order an image-preserving serializer needs), `unsafe map_put(hp, t, key, val, atoms)` / `unsafe map_remove(hp, t, key, atoms)` (functional).
- `pub(crate) map_pairs_flatmap_order(t, atoms)` (internal flat-map key order; what `compare`/`hashing` use).
- Non-boxed / non-map ⇒ `TermError::BadMap` (never a deref of an immediate).

### `fun.rs` — the on-heap fun / export term layout
A `make_fun3` local closure or an `EXPORT_EXT`/`NEW_FUN_EXT` literal (`erl_term.h:403-425`, `erl_fun.h:61-99`, `emu/instrs.tab:545-584`, `external.c:3966-4003/5006-5040/5102-5176`, `utils.c:2149-2210`). Layout in `src/fun.rs`.
- **Header word (C-exact):** bits 0..5 `FUN_SUBTAG` (`0x14`), 8..15 `arity`, 16..23 `num_free`, bit 24 `FUN_HEADER_KIND_OFFS` (C external), bit 25 `FUN_HEADER_IMPORTED_OFFS` (this crate's `NEW_FUN_EXT` marker).
- Constants: `FUN_HEADER_ARITY_OFFS=8`, `FUN_HEADER_ENV_SIZE_OFFS=16`, `FUN_HEADER_KIND_OFFS=24`, `FUN_HEADER_IMPORTED_OFFS=25`, `FUN_MAX_FIELD=0xFF`, `MAX_ARG=255`, `FUN_DESC_WORDS_LOCAL=2`, `FUN_DESC_WORDS_EXTERNAL=3`, `FUN_DESC_WORDS_IMPORTED=10`.
- `pub enum FunKind { Local, External, Imported }`; `pub struct FunPid { node: Atom, number, serial, creation: u32 }`.
- Pure (take a *header word*) helpers: `make_fun_header(arity, num_free, kind)->TermResult<Eterm>`, `fun_kind_of_header(hdr)->FunKind` (const), `fun_kind(hdr)->TermResult<FunKind>`, `fun_arity`/`fun_num_free` (const), `fun_desc_words(kind)`, `fun_words(kind, num_free)`, `fun_desc_words_header`/`fun_env_offset_header`/`fun_block_words_header`; `pub fn fun_block_words(t)->TermResult<usize>`.
- **A fun's true arity is out of band.** The header's 8-bit `arity` is what the C's `fun_info(F, arity)` reports (true arity truncated to its low byte); the C encoder emits the `Export`'s full `info.mfa.arity` (`external.c:3997-4001`). So the `External` descriptor is `[hdr][module][function][true arity]`; `write_fun_external` accepts **any** arity (masks the header with `FUN_MAX_FIELD`, `num_free == 0`, block `1 + 3`), and `fun_arity` remains the header reader. Oracle-verified: arity 256 reports `{arity,0}` from `fun_info/2` yet re-encodes 256.
- Heap-agnostic writers (caller reserves `fun_words(kind, num_free)`): `write_fun_local(hp, arity, uniq: u64, index: u32, env: &[Eterm])`, `write_fun_external(hp, arity, module: Atom, function: Atom)`, `write_fun_imported(hp, arity, module: Atom, uniq: &[u8;16], index, old_index, old_uniq: u32, creator: FunPid, env: &[Eterm])`.
- Accessors (wrong-kind/non-fun ⇒ `TermError::BadFun`): `fun_true_arity -> TermResult<usize>` (descriptor word 3 for `External`, the header field for `Local`/`Imported` — what an encoder/`compare`/hashing must use), `fun_uniq -> TermResult<u64>`, `fun_uniq_bytes -> TermResult<[u8;16]>`, `fun_index`/`fun_old_index`/`fun_old_uniq -> TermResult<u32>`, `fun_module`/`fun_function -> TermResult<Atom>`, `fun_creator_pid -> TermResult<FunPid>`, `fun_env -> TermResult<&[Eterm]>`.
- **Word layout:** `env[i]` at `hp[1 + desc_words(kind) + i]`, block = `1 + desc_words + num_free`:
  `Local` = `[hdr][uniq:u64][index:u32][env…]`;
  `External` = `[hdr][module:Atom][function:Atom][arity:usize raw]`;
  `Imported` = `[hdr][module][uniq0..8][uniq8..16][index][old_index][old_uniq][pid_node][pid_number][pid_serial][pid_creation][env…]`.
- `size.rs` sizes a fun `1 + desc_words + num_free`; `compare.rs` orders funs structurally (`Local` by `(uniq,index)` then `num_free` then env; `External` by `(module,function,arity)`; `Imported` by `(module,index,old_uniq)` then `num_free` then env); `term_hash` hashes exactly the fields `compare` compares.

### `record.rs` — native records (`ErtsRecordDefinition`/`ErtsRecordInstance`)
The OTP 29 native-record interior (`erl_record.h`/`erl_record.c`: `records_create_4`, `erts_canonical_record_def`, `records_get_2`, `records_get_field_names_1`, `records_is_exported_1`; `external.c` `RECORD_EXT`). Two boxed things (layout in `src/record.rs`):
- **Instance** (`2 + n` words), root `make_boxed(hp)`: `[0] make_record_header(n + 1)`, `[1] record_definition = make_boxed(def)` (tuple-tagged pointer), `[2..2+n]` field **values in sorted-key order**; `record_field_count = header_arity - 1`.
- **Definition** (`6 + n` words; its `thing_word` IS an arityval, so it reads as a tuple via `tuple_val`): `[0] make_arityval(n + 5)`, `[1] field_order` (tuple `[make_arityval(n)][rank…]`, `tuple0()` for `n == 0`), `[2] hash`, `[3] module`, `[4] name`, `[5] is_exported` (atom `true`/`false`), `[6..6+n]` field **keys in ascending atom-index order**; `field_order[declared_index]` = the field's rank in the sorted arrays.
- Construction block (what `records_create_4`/the ETF decoder allocate): `[field_order tuple (n+1, absent when n==0)][definition][instance]`.

Public surface: `RECORD_INST_FIXED_WORDS = 2` / `RECORD_DEF_FIXED_WORDS = 6` / `record_inst_size_words` / `record_def_size_words` / `record_block_words` (`const _: () = assert!` pins); `unsafe write_record(hp, module, name, is_exported, fields, definition_hash)` (`fields` in declaration order; sorts keys by `atom_val`, builds `field_order`, stores values sorted; duplicate/non-atom keys and arity overflow ⇒ `TermError::BadRecord`); readers (all `unsafe`, malformed ⇒ `BadRecord`): `record_field_count`, `record_definition`, `record_values`, `record_get(t, key, atoms)->TermResult<Option<Eterm>>` (linear scan via `compare::eq`), `record_is_native(t, module, name)`, `record_name_is(t, name)`, `record_definition_{field_count,module,name,is_exported,hash,keys,field_order}`, `record_declaration_keys(def)`. `AM_TRUE`/`AM_FALSE` (= `Atom 1`/`0`) exported for the `is_exported` word.

### `number.rs` — safe numeric → `f64`
- `pub fn big_to_f64(x: Eterm) -> Option<f64>` — faithful `big_to_double` (`big.c:2412-2529`): `bitlen = (limbs-1)*64 + msd_bits`, reject `bitlen > 1024`, top 54 bits rounded **ties-to-even**, scale by `2^exp`, apply sign. `Some` only for a finite representable value; `None` for a non-bignum or out-of-range magnitude (the C's `< 0` → `badarith`). The API `beam_engine`'s `Fconv` adopts.
- `pub fn number_as_f64(x: Eterm) -> f64` — total/panic-free: small/float → value, bignum → `big_to_f64` or `±f64::INFINITY`, non-number → `NaN`. **Best-effort** (nearest-double); NOT used for exact comparison (that is `compare/numbers.rs`).

### `error.rs`
- `pub enum TermError` / `pub type TermResult<T>` — this crate's error types, reconciled into `beam_common::BeamError` by an owned `From` impl.
- `TermGraphCyclic` / `TermGraphTooLarge` — from `size_object_bounded`; both map to `BeamError::BadArg`.
- `FunIdentityUnavailable` — `make_hash`/`make_hash2` on a `make_fun3` `Local` fun ⇒ `BadArg` (never a wrong hash).
- `RefIdentityUnavailable` — `make_hash`/`make_hash2` on an **internal reference** (the C mixes a container id this representation lacks) ⇒ `BadArg`; `term_hash` is unaffected.
- `BadRef` — a reference accessor applied to a non-M1-reference thing ⇒ `BadArg`; distinct from `RefIdentityUnavailable` and `NotATerm`.
- `NotATerm` — the word is not a term at all (see the non-term policy under Constraints).

## Atom-table snapshots — the O(1) append-only memo witness
`AtomSnapshot` + `AtomTable::{snapshot, appended_since, generation}` let a caller validate a memoized *image* of the table in **O(1)** (`src/atom.rs`; measured by `examples/atom_witness.rs`).
- **Contract.** `AtomTable` is append-only (`intern` its only mutating path), so an index — and any prefix — is stable for the table's lifetime. `appended_since(snapshot)` is `true` **only** when (1) `self` is the **same instance** as the snapshotting table, and (2) the length delta equals the generation delta (one name per generation bump). It never scans, never allocates, never returns a wrong `true`. Every index `< snapshot.len()` still names the same atom; names added since occupy indices `>= snapshot.len()`.
- **Memo protocol (both halves required).** Reuse `image` as an exact index→atom image of `shared` iff `shared.appended_since(snap) && image.len() <= shared.len()`, then extend with `shared`'s names at `image.len()..shared.len()`, re-taking `snap = shared.snapshot()` on every extension, and drop the memo if a decode fails part-way.
- **Adoption (outside this node).** `beam_bifs::binconv`'s `DECODE_SCRATCH` (`../../beam_bifs/src/binconv.rs:1105`) validates today via the O(table) `prefix_matches` (`:1089`; ~1.6–1.8 ns/atom, 5438 ns at 3009 atoms), making the ETF round trip verification-bound — it should become `RefCell<Option<(AtomTable, AtomSnapshot)>>` with the O(1) pair above. Measured here (`examples/atom_witness.rs`, release): the witness is **~0.5–0.9 ns/op flat from 4 to 30 009 atoms** vs 0.09 µs–1.22 ms for the `AtomTable::clone()` it replaces. In `beam_rt`, `CodeServer::atoms_mut()` (`../../beam_rt/src/codeserver.rs:293`) is `Rc::make_mut`, so a novel-atom intern mints a fresh instance and conservatively invalidates the memo (steady state does no `make_mut`, so the witness holds).

## Constraints
- **Bit-exact layout (Level-1 drop-in).** Every constant, mask, shift and encoding must match `../../erts/emulator/beam/erl_term.h` exactly.
- **Dependencies:** `beam_common` + `libc` only (foundation dependency sink). `beam_common` is used for exactly one thing: the `From<TermError> for beam_common::BeamError` impl in `error.rs` (owned here so every dependent gets `?`-interop for free); `TermError`/`TermResult` must not be redefined per dependent.
- **`unsafe` confinement:** `heap`, `literal`, `list`, `boxed`, `tuple`, `thing`, `fun`, `map`, `record`, `compare`, `hashing`, `ihash`, `bitstring`, `refc`, `number`. Every `unsafe fn` documents `# Safety`; every `unsafe` block carries `// SAFETY:`. `unsafe_op_in_unsafe_fn` stays clean.
- **A word that is not a term is never dereferenced.** `THE_NON_VALUE` (release `0`; debug `make_header(0, FLOAT_SUBTAG)`) has primary tag `HEADER`, which the C `is_boxed` bit trick *accepts*, so a bare-`is_boxed`-guarded deref could touch garbage. The guards are `tag::is_term_word(x) = !is_header(x)` and `boxed::is_boxed_term(x) = is_boxed(x) && is_term_word(x)` — **every deref/classification site tests one of them first**. Non-term policy: fallible APIs ⇒ `TermError::NotATerm` (`make_hash`/`make_hash2`); total APIs give a defined answer (`ops::class` ⇒ `NonValue`, `size_object` ⇒ `0`, `number_as_f64` ⇒ `NaN`, `term_hash` ⇒ a tagged raw-word hash); `compare::{cmp,eq,cmp_arith}` order a non-term **after every valid term** (never `Equal`; two non-terms by raw word), so `eq ⇒ equal hash` holds. The `pub unsafe fn *_parts` readers and `heap.rs` keep their "caller passes a live thing of this kind" contract.
- **Pure tag math is `const fn`** where the body allows (`raw`, `from_raw`, `primary_tag`, tag tests, `make_small`, `signed_value`, `make_atom`, `atom_val`, `make_arityval`, `arityval`, …).
- Files target ~1000 lines; one responsibility per file.

## Design Decisions
- **`Eterm` = `#[repr(transparent)]` newtype over `usize`** (not a raw alias): type safety + methods + a documented ABI contract while staying layout/pass-identical to `ERL_NIF_TERM` (Part I §A.3).
- **Fun descriptors inline the C `entry` pointer (an M1 substitution).** No fun/export table here, so the entry fields live *inside* the term (see the `fun.rs` layout). Bit 24 is the C kind bit; bit 25 (`FUN_HEADER_IMPORTED_OFFS`, ours, valid only while bit 24 is clear) discriminates the two local kinds, so a C reader's `is_external_fun` still sees a local fun. Carrying every wire field keeps decode→encode byte-exact and lets the fun be called (External ⇒ module+function+arity; Imported ⇒ module+index).
- **Checked + unchecked constructors.** `make_small`/`make_arityval` are unchecked (engine hot path, mirroring C); `try_*` variants return `TermResult` at trusted boundaries so range violations become errors, not silent corruption.
- **`Atom` is `u32`.** The representation allows 58-bit indices; the practical table is capped far below 2^32 (default `+t` limit 1,048,576). Atoms only come from `make_atom(Atom)`, so the encoding invariant holds.
- **`AtomTable` assigns indices in insertion order (0-based)**, reproducing `atom.names` (`../../erts/emulator/utils/make_tables`: `#define am_<alias> make_atom($i)`), required for preloaded modules that reference atoms positionally.
- **`AtomTable::clone` mints a new *identity*, and the O(1) witness depends on it.** Append-only + monotonic generation + a length check proves "same table, only appended to" *given* a way to tell two tables apart; a per-construction identity from a process-global counter supplies that. Preserving identity across `clone` would be unsound under copy-on-write (`Rc::make_mut` clones the live table on intern), so a clone takes a fresh one; the only cost is a conservative memo rebuild. The counter is bookkeeping, not observable behaviour, not part of any contract. The clone is also a **deep copy of the name storage** (a fresh `Arc<str>` per entry): sharing the source's `Arc`s would make a clone Θ(1) in allocations and silently hollow out the clone-as-sensitivity-control in `beam_rt/tests/atom_table_commit_alloc.rs` and `beam_bifs/tests/decode_hot_path.rs`; `Arc` (not `Rc`) preserves `Send + Sync`.
- **`heap::Heap` is a minimal term-construction arena, NOT the production allocator** (that is `beam_alloc`); it exists so the representation has a testable backing store. It never reallocates, so interior pointers never dangle.
- **`TUPLE0` is `tuple0()`, a function, not a `const`** — a shared empty-tuple literal must carry `TAG_LITERAL_PTR` (a pointer to a static block), and Rust rejects pointer→integer casts in const eval. Same reason `make_boxed`/`make_list`/`make_tuple` are plain `fn`.
- **Off-heap term materialization lives here (`thing.rs`), heap-agnostic.** This crate **cannot depend on `beam_alloc`** (dependency sink; `beam_alloc` already depends on `beam_term`), which is why every writer takes a caller-allocated `hp` and returns the tagged root; `beam_codec` keeps only error-wrapping + allocation and forwards the word layout.
- **The bit/float payload readers live in `bitstring.rs`** (one public definition each); `compare.rs` and `hashing/` forward, `thing.rs` re-exports `float_bits`/`float_value` — avoiding `ambiguous_glob_reexports`.
- **The per-object layout walk has a single definition (`src/layout.rs`).** The sizing and copy child enumerations are deliberately split because they diverge for `ErlSubBits` (sizing must NOT descend into `orig` — it would double-count; a copier MUST forward it).
- **`cmp_arith`'s mixed integer/float compare is exact (`src/compare/numbers.rs`)** — ports the C `mixed_types` block instead of rounding to `f64`; `double_to_big` is a private single definition there.

## Test Strategy
- Per-module `#[cfg(test)] mod tests` in every `src/*.rs` pins that thing's **raw words** (the layouts above; literal constants `HEADER_BIN_REF = 0xA8` / `HEADER_SUB_BITS = 0x124`; the 8-word `[BinRef][ErlSubBits]` pair rooted at `hp + 3`; the `MAX_REF_IDS` / single-digit / empty-flatmap / negative-bignum boundaries), plus write→read round-trips and typed rejections. `src/layout.rs` cross-checks `sum(object_words over the graph) == size_object`; `src/number.rs` pins hand-computed doubles, ties-to-even and the `bitlen` 1024/1025 boundary; `src/hashing/` carries the structural-hash + oracle-vector suites.
- `tests/layout.rs` (exact raw patterns vs `erl_term.h`: `make_small(0) == 0xF`, `make_atom(0) == 0xB`, `NIL == 0x3B`, `make_arityval(1) == 0x40`, cons tag `0x1`, plus boundary sweeps); `tests/abi.rs` (`size_of`/`align_of` = `usize`, `Eterm: Copy`, `repr(transparent)`); `tests/non_term.rs` (the crate-wide non-term policy over the public API).
- `tests/atom_witness.rs` (a counting `#[global_allocator]` proves `snapshot`/`appended_since` are allocation-free; a 4-vs-200 000-atom timing bound, plus clone-divergence / replaced-table / shrunken-table soundness at 50 000 atoms); `tests/size_bounded.rs` (cycles, budget inclusivity, non-terms ⇒ `Ok(0)`, a 100 000-element list, exact flat/HAMT sizes, per-occurrence counting); `tests/internal_ref.rs` (the M1 layout pinned literally — raw header `0xD0` — plus round-trip, typed errors, 4-word size, `=:=`/ordering); `tests/compare_terms.rs` / `tests/compare_external.rs` (ordering across class pairs; the external/record arms with a live-`erl` check); `tests/record_ref_hash.rs` (offline `(make_hash2, make_hash, definition_hash)` pins + a real `-record` fixture cross-check).
- Live-`erl` oracle cross-checks (**skip loudly, never fail**, when `erl` is absent; text protocol, because a dev-dependency on `beam_codec` would cycle): `tests/oracle.rs` (map size/get/pairs at the 31/32/33/40/100-key flat/hash boundary; bitstring parts/bits; bignum `eq`/`cmp`/`cmp_arith`/`size_object` across the 2-/3-limb boundary; flat-map stored key order vs `maps:to_list/1`); `tests/map_hash.rs` (HAMT pair-storage order: two offline-pinned vectors + 33/40/50/64/100-key mixed maps); `tests/number_oracle.rs` (`big_to_f64` vs `float/1` as IEEE-754 bits over a bit-length sweep + randomised limbs; `ERL_TIMEOUT = 60s`); `tests/compare_oracle.rs` (arithmetic/exact comparison vs `==`/`=:=`/`<`/`>` incl. the `>2^53` asymmetry).
- Exhaustive cross-checks against real ERTS belong to `tools/differential` (the compatibility gate). Iteration: `../../scripts/cargo.sh test -p beam_term`, `../../scripts/cargo.sh build -p beam_term`. Never `cd`.

## Routing Table
- `src/tag.rs` → tag constants/masks/shifts, `PrimaryTag`/`BoxedTag`, `primary_tag`, `is_header`, `is_term_word`
- `src/eterm.rs` → the `Eterm` word newtype, ABI contract
- `src/immediate.rs` → small ints, atom immediates, NIL, catch, pid/port immediates
- `src/atom.rs` → `Atom`, `AtomTable` (interning, `atom.names` seeding/parser, `hvalue`, the O(1) append-only witness)
- `src/list.rs` → cons-cell construction/access
- `src/boxed.rs` → boxed pointers, headers, arityval, sub-type predicates, map headers, the M1 internal-reference thing (`write_internal_ref`/`internal_ref_id`/`INTERNAL_REF_*`) and the C-layout reader `internal_ref_numbers`
- `src/tuple.rs` → tuple construction/size/element access (`tuple0`)
- `src/heap.rs` → minimal bump term arena
- `src/literal.rs` → `TAG_LITERAL_PTR` tagging + `tag_literal_area`/`set_literal_tag`/`LiteralArea`
- `src/ops.rs` → `TermClass`/`class`, numeric predicates, `THE_NON_VALUE`
- `src/compare.rs` → `cmp`/`eq`/`cmp_arith` (the `erts_cmp` port + deterministic fallbacks); `src/compare/numbers.rs` → exact `mixed_types` compare + private `double_to_big`; `src/compare/maps.rs` → the `MAP_SUBTAG` arm (`cmp_maps`: the allocation-free flat-vs-flat fast path over `thing::flatmap_parts` + the collecting `map_pairs_flatmap_order` fallback, gated by the O(n) `cmp_flatmap_keys` sortedness precondition)
- `src/hashing/mod.rs` → internal structural hash `term_hash`; `src/hashing/common.rs` → shared ERTS-hash primitives; `src/hashing/make_hash2.rs` → bit-exact `make_hash2` + `compute_record_definition_hash`; `src/hashing/make_hash.rs` → bit-exact `make_hash` + `hash_binary_bytes`
- `src/ihash.rs` → bit-exact `erts_map_hash`/`make_internal_hash` + `swizzle_map_hash`/`hashmap_nibble`
- `src/size.rs` → `size_object` + bounded, cycle-safe `size_object_bounded`
- `src/layout.rs` → `object_words` + `for_each_sizing_child_slot`/`for_each_child_slot` (the single layout source of truth)
- `src/thing.rs` → heap-agnostic float/bignum/flatmap/external-pid/port/ref blocks (the codec forwards here)
- `src/fun.rs` → the on-heap fun/export layout (`FunKind`/`FunPid`, `make_fun_header`/`fun_kind`/`fun_words`/`fun_block_words`, `write_fun_{local,external,imported}`, the `fun_*` accessors)
- `src/map.rs` → the Erlang map (flat-map/HAMT builder `write_map`, readers/lookups, `MAP_SMALL_MAP_LIMIT`, `make_map_header`/`map_words_capacity`)
- `src/record.rs` → native records (size fns, `write_record`, the `record_*`/`record_definition_*` readers)
- `src/number.rs` → `big_to_f64` + `number_as_f64`
- `src/bitstring.rs` → `ErlHeapBits`/`ErlSubBits` writers+readers, `bit_count_to_bytes`/`heap_bits_size_words`, `bitstring_parts`/`bit_at`, `float_bits`/`float_value`
- `src/refc.rs` → off-heap (`refc`) binaries (`BinRef`+`ErlSubBits` pair writers/accessors, `ERL_REFC_BITS_SIZE`/`ERL_ONHEAP_*`, `is_refc_binary`/`refc_*`)
- `src/error.rs` → `TermError`/`TermResult` + the `From<TermError> for beam_common::BeamError` interop impl
- `tests/` → crate-level integration tests (`layout.rs`, `abi.rs`, `non_term.rs`, `compare_terms.rs`, `compare_external.rs`, `size_bounded.rs`, `internal_ref.rs`, `atom_witness.rs`, `record_ref_hash.rs`, and the live-`erl` cross-checks `oracle.rs`/`map_hash.rs`/`number_oracle.rs`/`compare_oracle.rs`)
- `examples/atom_witness.rs` → witness micro-benchmark (`run --release --example atom_witness`)

## Handoffs (owned by other nodes — escalate writes to the parent)
- **The M1 internal-reference seam.** A **local-node** reference decoded from the wire (`NEWER_REFERENCE_EXT`/`NEW_REFERENCE_EXT`) is materialized by reserving `INTERNAL_REF_WORDS` words and calling `boxed::write_internal_ref(hp, id)` — never by hand-writing the words — and read back with `boxed::internal_ref_id(t)` (never `internal_ref_numbers`, which describes the C layout only). The wire form carries the local node's identity, which the M1 block does not store (`node`/`serial` are zeroed); a reference for **another** node is an external reference (`thing::write_external_ref`). **Parity:** the four words are byte-identical to `beam_process`'s `MonitorRef::to_eterm`, `beam_bifs`'s `make_ref_thing` and `beam_rt`'s native process floor; `tests/internal_ref.rs` pins that shape literally. Consumer owner: `beam_codec`.
- **`beam_codec` can carry an external fun arity > 255 and a zero-length external reference.** `fun::write_fun_external` accepts any arity (masks the header with `FUN_MAX_FIELD`; `num_free == 0` ⇒ block `fun_words(External, 0) == 4`, true arity in descriptor word 3, read by `fun_true_arity`), so an encoder should emit `EXPORT_EXT`'s `info.mfa.arity` via `fun_true_arity` and reserve `fun_words(External, 0)`. `thing::write_external_ref` accepts an **empty** `ids` slice. Owner: `beam_codec`.
- **Record / number follow-ups.** `beam_bifs` (`is_record/2,3`, `records:*`), `beam_codec` (`RECORD_EXT`, tag 67) and `beam_engine` (`Fconv`) can consume `record::*`, `number::big_to_f64` and `hashing::compute_record_definition_hash`; the last lets `records:create/4`/`update/4` compute the definition's `hash` word (the C's `make_hash2(TUPLE(module,name,is_exported,keys-in-declaration-order))`, `erl_record.c:1041-1046`) instead of the `make_small(0)` placeholder — `write_record` still takes that word as an argument. `compare`/`hashing`/`size` treat native records structurally.
- **`beam_message` may delegate its bounded walk to `size_object_bounded`.** `beam_message/src/copy.rs` re-implements a bounded, cycle-safe sizing walk (`bounded_words`, `is_acyclic`, `preflight_words`); `size::size_object_bounded` provides exactly that machinery. Owner: `beam_message`.

## Known Issues
- **Comparison gaps (`src/compare.rs`).** `cmp`/`eq`/`cmp_arith` faithfully reproduce `erts_cmp_compound` for atoms (by name), numbers, tuples, lists, bitstrings, maps (flat *and* hash — size first, then the `(key,value)` sequence in flat-map key order) and internal pids/ports; and structurally, field by field, external pids/ports/references and native records. Funs are ordered structurally (`cmp_funs`): `Local` by `(uniq, index)`, then `num_free`, then env; `External` by `(module, function, arity)`, after any local kind; `Imported` by `(module, index, old_uniq)`, then `num_free`, then env; a mixed `Local`/`Imported` pair leads with a kind rank (the `Local` descriptor carries no module — a documented deviation from the C). These still use a **deterministic, panic-free fallback** and must be extended for exact ordering: on-heap bin refs (class rank, then object arity, then raw term word); internal references (raw payload words — for the M1 block those words *are* the identity, so equal id ⇒ `=:=` and ids order numerically; for a C-layout block it is the packed `u32`s); atoms whose name is absent from the passed `AtomTable` (raw index). Hash-map comparison cannot be byte-matched to ERTS (ERTS's HAMT order depends on its hash), so the flat-map-key-order rule is used for both shapes.
- **`Atom` identity is table-relative, not global.** `Atom` is a bare index into one `AtomTable`; two atoms carry the same `Eterm` only if their table interned names in the same order. The `cmp_impl` fast path (`a.raw() == b.raw()` ⇒ `Equal`) means two atoms with the same index but different names (two tables) compare `Eq`; an out-of-range index falls back to raw-index comparison. Name-faithful only when operands and the passed table share one table — a per-module `AtomTable` (as `beam_loader::Module` builds) makes cross-module atom comparison unreliable. The fallback is the `_` arm of `cmp_atoms` (`src/compare.rs:361`); it silently breaks **ordering** only — equality stays faithful (two distinct atoms always carry distinct indices in one consistent table).
- **`term_hash` is internal, not compatibility-pinned (`src/hashing/mod.rs`)** — it guarantees only `eq(a,b,atoms) ⇒ term_hash(a,atoms) == term_hash(b,atoms)`. Maps hash structurally (sorted pairs); funs over exactly the fields `compare` compares; native records over exactly the fields `cmp_records` compares (the definition address is never mixed, so a record equals its copy hash-wise and `beam_ets`'s CHAMP invariant holds). External pid/port carry no pointer in their words, so their raw-word hash is content-stable; internal refs and on-heap bin refs use an opaque raw-word fallback (not stable across processes/images). One marginal `eq ⇒ equal hash` gap: `hash_words` mixes the external-ref block's header/arity while `cmp_external_refs` trims the longer id slice's leading **zero** ids, so two external refs of different id-array lengths whose extra ids are zero compare `Equal` yet hash differently — reachable only with a forged/foreign ref.
- **The HAMT is `erts_map_hash`-driven (`src/map.rs` + `src/ihash.rs`).** The trie is placed by the bit-exact `erts_map_hash` port, so a `write_map` hash map's stored pair order equals the oracle's `maps:to_list/1` order (`tests/map_hash.rs`). `swizzle_map_hash` is a nibble reversal; level `L` consumes the L-th **least-significant** nibble. Atom keys are placed correctly only when the `AtomTable` matches the oracle's indices (seeding from `atom.names` gives parity). The C hash is documented non-portable across VM instances ("Limitation #1", it mixes pointers); the pointer-derived classes use a documented deterministic substitute (`src/ihash.rs` docs).

## Notes for Agents
- **Read `../../erts/emulator/beam/erl_term.h` before changing any constant** — the ground truth; cite it where a value is non-obvious.
- **`TAG_LITERAL_PTR` contract.** `TAG_LITERAL_PTR` (bit `0x4`) sits *above* the 2-bit primary tag and *inside* the 3-bit `TAG_PTR_MASK` (0x7), so `boxed_val`/`list_val` resolution is unchanged by masking it off. The C enables it on 64-bit when there is no OS physical-memory reservation (the common case) or under DEBUG; we always define it (Level-1 fidelity). `is_literal` is a bare bit test for `0x4`, meaningful **only** on BOXED/LIST words (an IMMED1 word may have that bit set for unrelated reasons).
- `is_list(NIL)` is **false** — NIL is an `IMMED2` immediate; a "list that is not nil" is exactly a cons cell. `List` terms are single words (must stay so for the NIF/ETF/msg ABI).
- `make_arityval` encodes `size << 6` with subtag `0x00`, so arity 0 encodes to the raw word `0`; table cells/`[Eterm; N]` register files must not treat `0` as "empty" without care.
- **No fully safe dereferencing accessor layer.** Every accessor that reads a boxed pointer's header word is `pub unsafe fn` — `car`/`cdr`, `is_tuple`/`tuple_size`/`tuple_element`, `is_map`/`is_flatmap`/`is_hashmap`/`is_any_fun`, `is_float`/`float_value`/`float_bits`, `is_bitstring`/`is_binary`/`bitstring_size`, `class`/`is_integer`/`is_number`, and the `thing.rs` `*_parts` readers. Safe `fn`/`const fn` cover only raw-bit inspection (primary tag, immediates, `is_list`/`is_nil`, `is_boxed`/`is_header`, the `*_header` subtag tests, `header_subtag`/`map_header_*`/`arityval`, `boxed_val`/`list_val`/`tuple_val`) plus `cmp`/`eq`/`term_hash`/`size_object`/`AtomTable` access and the safe map readers `map::{map_size,map_get,map_pairs,map_pairs_storage_order}`. A `#![deny(unsafe_code)]` consumer must deref `*boxed_val(t)` itself to reach into a tuple/list/map/float/binary; header-word subtag tests take a *header word*, not a boxed term. The sibling-facing map/bitstring/refc/record APIs (`map::{write_map,map_put,map_remove}`, `bitstring::{bitstring_parts,bit_at,float_bits,float_value,write_heap_bits,...}`, `refc::{bin_ref_parts,refc_bytes,...}`, `record::{write_record,record_get,record_is_native,record_definition_*}`) are likewise `unsafe` (callers carry a `// SAFETY:` rationale). Exceptions: the safe map readers above and `number::{big_to_f64,number_as_f64}`.
- Interior layouts NOT externally pinned (list chunking, ETS tree shape) are open and documented at their own nodes. The map layout *is* owned here (`src/map.rs`) because `beam_gc` sizes it — keep it byte-compatible with the copier.

## See Also
- `../../CONTEXT.md` — workspace contracts (§2 frozen term layout, §3 error strategy).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.1–A.3, §B; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A, §B.
- `../../erts/emulator/beam/CONTEXT.md` — legacy core; its "Term Encoding (Eterm) Reference" section.
- `../beam_abi/` — NIF/driver C ABI; transmutes `Eterm` ↔ `ERL_NIF_TERM`.
- `../beam_alloc/`, `../beam_gc/` — production heap/allocator + collector built on `heap.rs`.
