# `beam_term` — Frozen BEAM Term Representation

## Intent
Owns the **frozen, bit-exact tagged term representation** of the Rust ERTS rewrite — the workspace's most load-bearing shared contract (workspace `CONTEXT.md` §2) — at **Level-1 drop-in fidelity**: a Rust core and the retained C periphery must not differ by one bit.
Ground truth: `../../erts/emulator/beam/erl_term.h` (plus `erl_map.h`/`erl_bits.h`/`erl_fun.h` for thing layouts) and `../../erts/emulator/utils/make_tables` for atom ordering.
Implements Part I §A.1–A.2 and Part II §B.
Every other crate depends downward on this one for term construction, inspection and classification; **no other crate may assume a different layout.**
Detail beyond this file lives in five child nodes: `src/compare/`, `src/hashing/`, `src/size/`, `tests/`, `examples/`.

## The frozen layout (what every dependent relies on)
- `Eterm` is **one machine word**: a `#[repr(transparent)]` newtype over `usize`, ABI-identical to `ERL_NIF_TERM` (`beam_abi` transmutes between them); no other crate may redefine it.
- **2-bit primary tag**: `HEADER=0x0` / `LIST=0x1` / `BOXED=0x2` / `IMMED1=0x3`.
- **3-bit pointer tag** on 64-bit: `TAG_PTR_MASK=0x7`, whose bit `0x4` is `TAG_LITERAL_PTR` (an off-heap literal-area pointer). It sits *above* the primary tag, so masking it off leaves `boxed_val`/`list_val` unchanged; it is always defined here (the C enables it on 64-bit without an OS physical-memory reservation, or under DEBUG), and `is_literal` is a bare bit test meaningful **only** on BOXED/LIST words — an IMMED1 word may have that bit set for unrelated reasons.
- **Small integers are 60-bit immediates** (`SMALL_BITS=60`, `MAX_SMALL=(1<<59)-1`, IMMED1 tag `0xF`); **atoms are 6-bit-shifted immediates** (`ATOM_INDEX_BITS=6` inside IMMED2: mask `0x3F`, `ATOM=0xB`, `MAX_ATOM_INDEX=2^58-1`).
- **A cons cell is a header-less 2-word `[CAR, CDR]` tagged `TAG_PRIMARY_LIST`** — no header word, so "a list that is not nil" is exactly a cons cell and a `List` term stays one word (required by the NIF/ETF/message ABI).
- `NIL` is the IMMED2 immediate `0x3B`; `is_list(NIL)` is **false**.
- **Headers:** `HEADER_ARITY_OFFS=6`, `HEADER_SUBTAG_MASK=0x3C`, `MAX_ARITYVAL=(1<<24)-1`, 14 `BoxedTag` subtags.
- **Frozen immediate widths:** `PID_NUM_SIZE=28`, `PID_DATA_SIZE=60`, `PID_SER_SIZE=32`, `PORT_DATA_SIZE=60`.

`src/tag.rs` / `src/immediate.rs` are the authority for the full constant list.

## API Surface (FROZEN — every downstream crate routes here)
Only the frozen *constants*, *layouts* and *invariants* are stated here; each module (and the Routing Table) owns its item list, and every item is `pub` (or `pub unsafe fn` where the signature says so).

### `eterm.rs`
`pub struct Eterm(usize)` — `#[repr(transparent)]`, `Copy + Eq + Hash + Debug`, `Default` → `NIL`; `pub const fn raw`/`from_raw`/`nil`/`non_value`.

### `boxed.rs`
Boxed pointers, headers, the arityval family, the `MAP_HEADER_*` accessors, `make_record_header`, and macro-generated `unsafe` kind predicates over the boxed subtags (`is_map`/`is_float`/`is_big`/`is_internal_ref`/`is_record`/`is_bitstring`/`is_external_*`/`is_*_fun` …), each with a `const fn` `*_header` twin taking a *header word*; rustdoc lists the full predicate set.
- **M1 internal-reference thing (canonical layout):** frozen 4-word `[make_header(3, REF_SUBTAG), node, serial, id]`, `INTERNAL_REF_WORDS = 4`, written only by `unsafe write_internal_ref(hp, id)` and read only by `unsafe internal_ref_id(t)` (`BadRef` for a non-term, another boxed kind, a C-layout arity-2 block, or a non-small id word).
  The whole identity rides that one 60-bit id word (`SmallIntOutOfRange` beyond `0..=2^59-1`); ERTS's own id is a full 64-bit `Uint64` across three wire numbers, so widening is a term-layout break — hence `node`/`serial` stay zeroed and a bignum id would need a variable-size thing.
- `unsafe internal_ref_numbers<'a>(t) -> &'a [u32]` is the **C-layout reader** only; nothing calls it and it must **never** be fed an M1 reference (it would return small-int tags, not ids).

### `literal.rs` (off-heap literal areas, `TAG_LITERAL_PTR`)
`make_literal`, `is_literal`, `clear_literal`; `tag_literal_area(&mut [Eterm], &mut [Eterm])` is the **safe slice-based** form and `unsafe set_literal_tag` its raw-pointer shim (the `erts_set_literal_tag` graph walk); `LiteralArea` is the bump-allocated frozen block.
The shared `tag_words` walk steps over a HEADER body by `thing_arityval` — except a **fun** header, by `fun_desc_words(fun_kind_of_header(hdr))` (2/3/10), so the inlined descriptor words are never tagged, and `HEADER_SUB_BITS`, by `thing_arityval - 1` (the `orig` field).

### `compare.rs` (`erts_cmp`) — arms in `src/compare/CONTEXT.md`
LCA ordering for `beam_ets` ordered_set, the `beam_bifs` comparison BIFs and `<`/`>` (`erts_cmp`).
`class_rank` fixes the cross-class ladder `small < big < float < atom < external_ref < ref < fun < external_port < port < external_pid < pid < tuple < record < map < nil < list < bitstring < bin_ref < non-value` (`rank = 0x10 - _DEF`, `erl_term.h:1441-1458`).
- `cmp(a, b, atoms)` is the exact (`=:=`) order (`exact=1`), `eq` its `.is_eq()`; `cmp_arith` is the arithmetic (`==`) order (`exact=0`, differing only for floats and mixed int/float pairs) and uses the C's **exact** mixed algorithm, not an `f64` cast: `1 == 1.0`, `-0.0 == +0.0`, `9007199254740993 > 9007199254740992.0`, NaN/±inf deterministic.
- `cmp_flatmap_keys` (`erl_utils.h:267-272`) — atoms by index, anything else by `cmp` — is the order a flat map stores its keys in.
- **`eq` answers atom-vs-atom by index, not by name** (equality only; `cmp` still orders atoms by name, as `erts_cmp_atoms` requires) — within one `AtomTable` interning is name-unique, so two different atom words are never `=:=`.
- **Maps compare structurally: size first, then the `(key, value)` sequence in flat-map key order**, and that key order is **canonical and context-free** — a map's stored order never depends on which node asks.
- **Same-class structural arms** (external pids/ports/refs, native records) compare field-by-field in the C's order, so a **copy is `=:=` its original**.
- **Funs compare structurally in the C's order (`cmp_funs`):** `Local` by `(uniq, index)` then `num_free` then env; `External` by `(module, function, arity)` after any local kind; `Imported` by `(module, index, old_uniq)` then `num_free` then env; a mixed `Local`/`Imported` pair leads with a kind rank (the `Local` descriptor carries no module — a documented deviation from the C).
- **Local-node-aware entry points:** `cmp_with_local_node`/`cmp_arith_with_local_node` take a `compare::LocalNodeId { sysname: Eterm (an atom), creation: u32 }` naming `erts_this_node`; `cmp`/`cmp_arith` are the `local: None` forms.
  With a `local`, a mixed internal/external pair of the *same* category is decided **before** the rank shortcut, field by field (`CmpCtx { atoms, exact, local }` threads through `cmp_impl`); with `local: None` it stays **rank**-ordered — the documented no-context convention — and both entry points are byte-identical to `cmp`/`cmp_arith` for every other pair.
Arm-level semantics, the field-precedence tables and the adopter/non-adopter handoff: `src/compare/CONTEXT.md`.

### `hashing/` — pinned `phash2`/`phash` + internal hash; detail in `src/hashing/CONTEXT.md`
`make_hash2(t, atoms)` / `make_hash(t, atoms)` (`-> TermResult<u32>`) are **bit-exact u32-wrapping ports** of `erl_term_hashing.c` — the values `erlang:phash2/1,2` and `erlang:phash/2` return — and, unlike `term_hash`, **these ARE part of the Level-1 drop-in contract** (oracle-cross-checked).
`term_hash(a, atoms) -> u64` is the crate-internal structural hash (`eq(a,b,atoms) ⇒ equal hashes`; **its exact values are not part of any contract**) — structural over maps, funs and native records in exactly the fields `compare` compares.
`compute_record_definition_hash(def, atoms)` is the synthetic definition hash the C caches; both ports mix a record's **real computed** hash, never the `definition_hash` word the block stores, and walk values in sorted-key order.
The `LocalFunIdentity` provider seam supplies the one field a `Local` fun's descriptor cannot carry (its defining module); no provider is a typed `FunIdentityUnavailable`, never a substituted hash.

### `ihash.rs` — bit-exact `erts_map_hash`
The Erlang map hash (`erl_term_hashing.c:1955-2130`) plus `swizzle_map_hash` (nibble reversal) and `hashmap_nibble` (the level-`L` least-significant nibble); it drives `map.rs` leaf placement, so a `write_map` hash map's stored pair order equals the oracle's `maps:to_list/1` order.

### `layout.rs` — canonical per-object layout (single source of truth)
How many heap words an object occupies and which payload words a copier must forward; `size.rs` consumes it, `beam_message::layout` re-exports the walk, and `beam_gc::copy::move_size` is `object_words` plus the copy-specific `ErlSubBits` override.
- `object_words(header, ptr)` (header included): tuple/record `arity + 1`; fun `1 + fun_desc_words(kind) + fun_num_free`; flatmap `size + 3`; HAMT node `1 + header_arity + popcount(MAP_HEADER_VAL)`; other things `thing_arityval + 1`; `ErlSubBits` → `ERL_REFC_BITS_SIZE` (8) when `orig` is a `BinRef`, `heap_bits_size_words(end - start)` for a match context, else `ERL_SUB_BITS_SIZE` (5).
  `object_words` is **purely structural**; the one pre-policy skip a sizer applies is the literal-tagged empty tuple (`tuple0()`), which reports `1` here but `0` from `size_object` (the global literal, `copy.c:125-140`). A zero-arity `RECORD` header is **not** that literal (the C `ASSERT` at `copy.c:128` excludes it) and reports `1` in both places.
- `for_each_sizing_child_slot` and `for_each_child_slot` differ **only** for `ErlSubBits`: the sizer gets no child (descending into `orig` would double-count), the copier gets its `orig` word (`SUB_BITS_ORIG_WORD` = 4) because the term must be forwarded.
- Neither enumerator has a `HEAP_BITS_SUBTAG`/`BIN_REF_SUBTAG` arm — both fall to the catch-all leaf, so an `ErlHeapBits` (raw byte payload) and a `BinRef` (`[hdr, val, next]`) expose no child slots, and a walker reaches the payload pointer only by testing the header itself.
- Both enumerators take a **header word at a pointer** (offsets relative to offset `0` = the header) and require the caller to have applied the crate's deref guard first; a `TAG_PRIMARY_LIST` cons cell has no header word and is not part of this API.
- **Words `1`/`2` of an external pid/port/ref are not terms in the generic sense:** word `1` is the sysname atom immediate, but word `2` is `Eterm::from_raw(creation as usize)` — not a term — and an external ref's data section is packed `u32`s.
  The enumerators therefore expose no child slots for these subtags and `object_words` prices them as `thing_arityval + 1`; any generic word walker (copier, encoder, GC pass) must copy them **verbatim** and never classify word `2` or an id word as a term.

### `size.rs` — walk detail in `src/size/CONTEXT.md`
`size_object(a)` — flat heap words the graph occupies: `0` immediates, `2` per cons cell, `arity + 1` per boxed object, plus nested sizes; bounded, so a cyclic/forged graph gives a defined partial count.
`size_object_bounded(a, limit)` — `Ok(n)` iff acyclic and `n <= limit`, `Err(TermGraphTooLarge)` over limit, `Err(TermGraphCyclic)` for a cycle (priority over the limit); one explicit-stack, **path-based** DFS over `layout`'s interior, so a shared-but-acyclic subterm counts once per occurrence and a non-term word contributes `0` and is never dereferenced.
`SizingPolicy` (`DefaultSizingPolicy` = both defaults) is the caller-supplied half of the C `size_object_x(t, litopt)` (`global.h:1135/1136`), with exactly two independent, object-safe axes:
- `is_shared(t)` prices a subterm the consumer reuses **by identity** at `0` words and never descends into it (the `litopt` literal skip, `copy.c:99`/`:119`);
- `object_words(header, payload) -> Option<usize>` overrides one boxed object's own words (`None` = prices nothing and exposes no child; default = `layout::object_words`) — what lets a copier price a refc binary it must flatten as its window `heap_bits_size_words(end - start)` instead of the 8-word pair.
`size_object_with(a, &dyn SizingPolicy)` / `size_object_bounded_with(a, limit, &dyn SizingPolicy)` are the parameterised walks; `size_object`/`size_object_bounded` are exactly the `&DefaultSizingPolicy` case, so the seam is unobservable to them.
A policy changes *pricing* only: child slots stay `layout::for_each_sizing_child_slot`'s answer, and nothing can **under**-count (the dangerous direction for a copier) — the sole pre-policy skip is the one `layout.rs` documents above.
`size_object_with_scratch`/`size_object_bounded_with_scratch` take a caller-owned `SizingScratch` and are allocation-free once it is warm; `src/size/CONTEXT.md` owns that contract and the per-call cost of the walk.

### `thing.rs` — off-heap term materialization (heap-agnostic)
Writers/readers for the boxed "thing" families this crate owns (float, bignum, flat map, external pid/port/ref — `ExternalThing`), byte-identical to the oracle-verified `beam_codec::term`, which forwards here.
**Every writer takes a caller-allocated `hp: *mut Eterm`** (`beam_alloc` in production, `heap::Heap` in tests) and returns the tagged root — this crate cannot allocate.
- Float `FLOAT_SIZE_WORDS = 2`; bignum `big_size_words(n)` = `n + 1` (one `ErtsDigit`=`u64` per word, LS-first, sign in `_BIG_SIGN_BIT`); flat map `flatmap_size_words(n)` = `3` empty else `4 + 2n`, block `[arityval(n), k0..k(n-1), MAP_HEADER_FLATMAP, n, keys-tuple, v0..v(n-1)]`, root `make_boxed(hp + 1 + n)`.
- External things (incl. the M1 "`next` holds `creation`" substitution): `MAX_REF_IDS = 5`; pid/port blocks are 4 words, `external_ref_size_words(n_ids)` = `3 + n_ids/2 + 1` on `ARCH_64`. **`ids` may be empty**: a zero-id external reference round-trips (`[header, node, creation, count = 0]`, `external_ref_size_words(0) == 4`, hashing mixes the zeroed word after count ⇒ `0`).

### `bitstring.rs` — bit-level payloads
`ErlHeapBits`/`ErlSubBits` writers+readers plus the public bit/float payload helpers; bits are **MSB-first inside each byte**, the last byte's unused trailing bits are zeroed, **non-byte-aligned shapes are supported**, and `ERL_SUB_BITS_FLAG_MASK = 0x3` (`_MUTABLE`/`_VOLATILE`).
`bit_count_to_bytes(n_bits)` = `ceil(n_bits/8)`, `heap_bits_size_words(n_bits)` = `2 + ceil(ceil(n_bits/8)/8)`; `write_heap_bits` builds `[make_header(words-1, HEAP_BITS_SUBTAG), n_bits, data…]`, rejecting (`BadBitstring`) a `bytes` shorter than `ceil(n_bits/8)`; `write_binary`/`write_sub_bits`/`sub_bits_parts` cover the 5-word `ErlSubBits` block.
`bitstring_parts` returns (base, **bit** offset, **bit** size); `bit_at`/`float_bits`/`float_value` live here as the crate's single float readers (`compare.rs` and `hashing/` forward).

### `refc.rs` — off-heap (`refc`) binaries
The on-heap `BinRef` + `ErlSubBits` pair for a binary/bitstring above the on-heap limits: the 8-word arrangement `[BinRef(3)][ErlSubBits(5)]` with the tagged root at `hp + 3`; `ERL_REFC_BITS_SIZE = 8`, `ERL_ONHEAP_BINARY_LIMIT = 64` bytes, `ERL_ONHEAP_BITS_LIMIT = 512` bits; `refc_offset`/`refc_size` report **bits**.
- **Frozen inter-crate contract (`beam_alloc`/`beam_gc`):** `BinRef.val` is a raw pointer to the payload's **first data byte** and `BinRef.val == RefcBinary::data_ptr()` (the off-heap header is `beam_alloc`'s concern); `ErlSubBits.orig` is a term (a boxed `BinRef`, or an on-heap bitstring for a match context); the 8 words move/copy together with `orig` fixed up to the new `BinRef`.
- **`orig` never names another `ErlSubBits`** — the C `ASSERT(erl_sub_bits_is_match_context(_sb) || _br)` (`erl_bits.h:385`) forces a non-match-context sub-bits to point at a `BinRef` at all times.
  For a refc pair `orig` is `make_boxed(hp)`, a pointer *backwards* into the same block, so a walk that follows it revisits the same pair and never reaches a second payload; the slot enumerator reports that word for **every** `SUB_BITS` shape (and performs a discarded read of word 4 before `visit(4)`), so a walker must type-check the word it reads — a standalone/malformed sub-bits may have an immediate `orig`, and word 4 must be readable in any case.
- **The pair root vs the handle is `HEADER_SUB_BITS` vs `HEADER_BIN_REF`:** `refc::is_refc_binary(t)` is "`SUB_BITS` header whose `orig` is a `BinRef`", and the **payload pointer lives only in `BinRef` word 1**.
  `ErlSubBits.base_flags` (word 1) stores a data base too, but the C follows `br->val->orig_bytes` whenever the binary may have moved; `refc_bytes`/`bitstring_parts` read `base_flags` and are therefore correct only while the payload cannot move (this crate never installs `ERL_SUB_BITS_FLAG_VOLATILE`), so a payload roster must key on `BinRef.val`.
  `bin_ref_parts`' `val` is a `*mut Eterm` *by type only* — it addresses the payload's first **byte** and must never be dereferenced as a term word.

### `map.rs` — flat map + HAMT/CHAMP builder, lookups, size
The Erlang map is a boxed `MAP_SUBTAG` term: flat (`MAP_HEADER_FLATMAP`, written by `thing.rs`) for ≤ `MAP_SMALL_MAP_LIMIT = 32` pairs, else a **hash map** — a 16-way (`4` hash bits/level, `HAMT_MAX_LEVEL = 16`) CHAMP whose level `L` selects the **`L`-th least-significant** nibble of the key's `erts_map_hash`.
- Trie node layouts (frozen; what `beam_gc::copy::move_size` sizes): head-array `[hdr][size][16 slots]` (18 words), head-bitmap `[hdr(bmp)][size][n slots]` (`2+n`), inner bitmap `[hdr(bmp)][n slots]` (`1+n`), collision node = a `make_arityval(n)` tuple of `n` key/value cons cells; one **term per slot**.
- `map_words_capacity(n)` = `flatmap_size_words(n)` at `n <= 32`, else `21n + 18` — a **sound** upper bound on the words `write_map`/`map_put` write, and the reservation each asks its caller to make. It is deliberately loose (the write is one contiguous prefix, so the unwritten tail costs address space, not memory); `tests/map_words_capacity.rs` owns the measured reservation/written ratios and the untouched-tail guard.
- `write_map(hp, pairs, atoms)` dedups (last wins) with **one stable sort plus one linear pass collapsing adjacent `eq`-equal runs** (`collapse_eq_runs`: a run keeps its first key word and takes its last value) — the C `erts_validate_and_sort_flatmap` shape, never a per-insertion scan.
  ≤ 32 input pairs is necessarily flat (dedup can only shrink): ONE buffer copied, sorted by `cmp_flatmap_keys`, collapsed, never `erts_map_hash`; above it each key is hashed once, leaves sorted by `swizzle_map_hash(hash)` (the C's `hxnodecmp` order, equal-hash runs broken by `cmp`) and collapsed, going flat if the collapse lands ≤ the limit — so the stored order is **canonical**, equals `maps:to_list/1`, and is insertion-order free.
- Readers `map_size`/`map_get`/`map_pairs` (sorted by `compare::cmp`)/`map_pairs_storage_order` (the *stored* order an image-preserving serializer needs) plus the functional `map_put`/`map_remove`; a non-boxed/non-map term ⇒ `TermError::BadMap`.

### `fun.rs` — the on-heap fun / export term layout
A `make_fun3` local closure or an `EXPORT_EXT`/`NEW_FUN_EXT` literal (`erl_term.h:403-425`, `erl_fun.h:61-99`, `external.c:3966-4003/5006-5040/5102-5176`, `utils.c:2149-2210`); `pub enum FunKind { Local, External, Imported }`.
- **Header word (C-exact):** bits 0..5 `FUN_SUBTAG` (`0x14`), 8..15 `arity`, 16..23 `num_free`, bit 24 `FUN_HEADER_KIND_OFFS` (C external), bit 25 `FUN_HEADER_IMPORTED_OFFS` (this crate's `NEW_FUN_EXT` marker, valid only while bit 24 is clear, so a C reader's `is_external_fun` still sees a local fun); `desc_words` = `2`/`3`/`10` for `Local`/`External`/`Imported`.
- **Word layout:** `env[i]` at `hp[1 + desc_words(kind) + i]`, block = `1 + desc_words + num_free`; `Local` = `[hdr][uniq:u64][index:u32][env…]`; `External` = `[hdr][module][function][arity raw]`; `Imported` = `[hdr][module][uniq[0..8]][uniq[8..16]][index][old_index][old_uniq][pid_node][pid_number][pid_serial][pid_creation][env…]`.
  The C's out-of-line `ErlFunEntry`/`Export` `entry` pointer is **inlined into the term** (the documented M1 substitution).
- **A fun's true arity is out of band.** The header's 8-bit `arity` is what `fun_info(F, arity)` reports, but the C encoder emits the `Export`'s full arity, so the `External` descriptor is `[hdr][module][function][true arity]` and `write_fun_external` accepts **any** arity (masking the header, `num_free == 0`); oracle-verified: arity 256 reports `{arity,0}` yet re-encodes 256.
  `fun_true_arity` is what an encoder/`compare`/hashing must use; it accepts every fun kind (`External` ⇒ descriptor word 3, `Local`/`Imported` ⇒ the header's 8-bit `fun_arity`), and only a non-fun ⇒ `TermError::BadFun`.
- **`fun_local_key(t) -> TermResult<(u64, u32)>` — ONE `(uniq, index)` lambda identity for both encodings of one lambda.** `FunKind::Local` reads descriptor words 1/2, `FunKind::Imported` word 6 (`old_uniq`) and word 4 (`index`); `FunKind::External` (an `M:F/A`, not a lambda entry) ⇒ `TermError::BadFun`, as does a non-fun.
  Both name the same `FunT` entry because `lib/compiler/src/beam_asm.erl`'s `finalize_fun_table/2` stamps the 27-bit MD5 prefix into each entry's `old_uniq` (and `emu_load.c:645-651` keys on it), so one lookup resolves a `make_fun3` closure and its `NEW_FUN_EXT` image alike (`beam_rt`'s `local_fun_target`/`imported_fun_target`).

### `record.rs` — native records (`ErtsRecordDefinition`/`ErtsRecordInstance`)
The OTP 29 native-record interior, two boxed things:
- **Instance**, `2 + n` words: `[0] make_record_header(n + 1)`, `[1] record_definition = make_boxed(def)`, `[2..2+n]` field **values in sorted-key order**.
- **Definition**, `6 + n` words, its `thing_word` an arityval (so it reads as a tuple): `[0] make_arityval(n + 5)`, `[1] field_order` (tuple of `n` ranks, `tuple0()` when `n == 0`), `[2] hash`, `[3] module`, `[4] name`, `[5] is_exported`, `[6..6+n]` field **keys in ascending atom-index order**; `field_order[declared_index]` = that field's rank in the sorted arrays.
  `RECORD_INST_FIXED_WORDS = 2`/`RECORD_DEF_FIXED_WORDS = 6` are `const`-assertion-pinned and the construction block is `[field_order tuple][definition][instance]`.
- The invariant both halves exist for: field lookup, `=:=` compare and hash all walk **sorted key order**, while the caller's API is declaration order.
  `write_record` sorts the caller's `fields` (duplicate/non-atom keys and arity overflow ⇒ `TermError::BadRecord`); the `record_*`/`record_definition_*` readers are `unsafe` and reject a malformed block with `BadRecord`.

### `number.rs` — safe numeric → `f64`
`big_to_f64(x) -> Option<f64>` — faithful `big_to_double`: `bitlen = (limbs-1)*64 + msd_bits`, reject `bitlen > 1024`, top 54 bits rounded **ties-to-even**, apply sign; `Some` only for a finite representable value, `None` for a non-bignum or out-of-range magnitude (the C's `< 0` → `badarith`).
`number_as_f64(x) -> f64` — total/panic-free but **best-effort** (bignum → `big_to_f64` or `±f64::INFINITY`, non-number → `NaN`), never used for exact comparison (that is `compare/numbers.rs`).

### `atom.rs` — atoms and the atom table
`pub struct Atom(u32)` (`Atom::MAX` = `u32::MAX`); the representation allows 58-bit indices and the practical table is capped far below 2^32 (default `+t` limit 1,048,576).
`AtomTable` assigns indices **in insertion order from 0**, one per distinct name, reproducing `atom.names` — preloaded modules reference atoms positionally, so the order is load-bearing.
- **Backing store** `{ names: Vec<Arc<str>>, index: HashMap<Arc<str>, Atom>, instance: u64, generation: u64 }`: a name is stored **once** as an `Arc<str>` (`Arc`, not `Rc`, so the table stays `Send + Sync`), so `name` is **O(1)**, `intern` allocates **exactly one** `Arc<str>` for a novel name and **nothing** for a known one.
- `hvalue` is the C `atom_tab(a)->slot.bucket.hvalue` (Dragon-Book `hashpjw` over the UTF-8 name bytes + the latin1 clutch, `atom.c:82`) — the value `make_hash`/`make_hash2` mix for an atom; out-of-range falls back to `a.raw()`.
- `fork(&self) -> AtomTable` is the Θ(1)-allocation whole-table copy for a consumer that may append but must not deep-copy Θ(N) strings: it shares every `Arc<str>` handle while keeping every index and the `generation`.
  `Clone` stays the deliberate deep copy (a fresh `Arc<str>` per entry) and is **not** interchangeable with `fork` — sibling exact-count pins use it as a Θ(N) sensitivity control.
  `fork` is ≈13× cheaper in retired instructions, flat in table size, and allocates 2 blocks at every size (0 for an empty table) against `clone`'s one string per atom; `examples/atom_fork_bench.rs` reproduces both figures, and sharing the two container allocations themselves is a deliberate non-goal.

#### Atom-table snapshots — the O(1) append-only memo witness
`AtomSnapshot` + `AtomTable::{snapshot, appended_since, generation}` let a caller validate a memoized *image* of the table in **O(1)**.
- **Contract.** `AtomTable` is append-only (`intern` its only mutating path), so an index — and any prefix — is stable for the table's lifetime.
  `appended_since(snapshot)` is `true` **only** when `self` is the **same instance** as the snapshotting table *and* the length delta equals the generation delta (one name per generation bump); it never scans, never allocates, never returns a wrong `true`.
  Every index `< snapshot.len()` still names the same atom; names added since occupy indices `>= snapshot.len()`. `AtomSnapshot` exposes `len()`/`is_empty()`/`generation()`.
- **Memo protocol (both halves required).** Reuse `image` as an exact index→atom image of `shared` iff `shared.appended_since(snap) && image.len() <= shared.len()`, extend with `shared`'s names at `image.len()..shared.len()` re-taking `snap` on every extension, and drop the memo if a decode fails part-way.
- **Adoption (outside this node).** `beam_bifs::binconv`'s per-thread `DECODE_SCRATCH` validates its memoized image exactly this way, with the O(table) `prefix_matches` surviving only as the fallback; `beam_rt`'s `CodeServer::atoms_mut` is `Rc::make_mut`, so a novel-atom intern conservatively invalidates the memo (steady state does no `make_mut`, so the witness holds).

### `error.rs`
`pub enum TermError` / `pub type TermResult<T>`, reconciled into `beam_common::BeamError` by an owned `From` impl (`?`-interop for every dependent): `SmallIntOutOfRange` ⇒ `BadArith`; `ArityOutOfRange`/`AtomIndexOutOfRange` ⇒ `SystemLimit`; `HeapExhausted` ⇒ `Alloc(..)`; everything else ⇒ `BadArg`.
- `PidIdOutOfRange` (a rejected internal pid number/serial ⇒ `badarg`, the oracle's own answer at both C call sites) · `BadMap`/`BadBitstring`/`BadFun`/`BadRecord`/`InvalidAtomName` (named for the family that rejects them) · `BadRef` (a reference accessor on a non-M1-reference thing, distinct from `RefIdentityUnavailable` and `NotATerm`).
- Capability gaps that must never become a wrong value: `FunIdentityUnavailable`/`RefIdentityUnavailable` (`make_hash`/`make_hash2` only — `term_hash` is unaffected); `TermGraphCyclic`/`TermGraphTooLarge` from `size_object_bounded`.
- **`make_pid_data` is unchecked by design** — it mirrors the C macro (`erl_term.h:655-656`: `(ser << PID_NUM_SIZE) | num`, no masking, only `debug_assert`ed) because every C call site range-checks instead (`external.c:3176`, `bif.c:4582`); masking would *deviate*, since the C's `|` lets an overflowing number carry into the serial.
  `try_make_pid_data(ser, num) -> TermResult<u64>` is that call-site check as one seam, applied **before** the fields are combined, rejecting against `MAX_INTERNAL_PID_DATA`/`_PID_NUMBER`/`_PID_SERIAL`.

## Constraints
- **Bit-exact layout (Level-1 drop-in).** Every constant, mask, shift and encoding must match `../../erts/emulator/beam/erl_term.h` exactly.
- **Dependencies:** `beam_common` + `libc` only (foundation dependency sink).
  `beam_common` is used for exactly one thing — the `From<TermError> for beam_common::BeamError` impl in `error.rs`, owned here so every dependent gets `?`-interop for free; `TermError`/`TermResult` must not be redefined per dependent.
- **The crate cannot allocate.** Every term writer is heap-agnostic: the caller reserves the words and passes `hp: *mut Eterm`; allocation itself lives in `beam_alloc` (production) or `heap::Heap` (tests).
- **`unsafe` confinement:** the term-construction/deref modules, the `map`/`record`/`fun`/`bitstring`/`refc` writers, comparison, hashing and the numeric reader — rustdoc is the authoritative per-module list.
  Every `unsafe fn` documents `# Safety`; every `unsafe` block carries `// SAFETY:`; `unsafe_op_in_unsafe_fn` stays clean.
- **A word that is not a term is never dereferenced.** `THE_NON_VALUE` (release `0`; debug `make_header(0, FLOAT_SUBTAG)`) has primary tag `HEADER`, which the C `is_boxed` bit trick *accepts*, so a bare-`is_boxed`-guarded deref could touch garbage.
  The guards are `tag::is_term_word(x) = !is_header(x)` and `boxed::is_boxed_term(x) = is_boxed(x) && is_term_word(x)` — **every deref/classification site tests one of them first**.
  Non-term policy: fallible APIs ⇒ `TermError::NotATerm` (`make_hash`/`make_hash2`); total APIs give a defined answer (`ops::class` ⇒ `NonValue`, `size_object` ⇒ `0`, `number_as_f64` ⇒ `NaN`, `term_hash` ⇒ a tagged raw-word hash); `compare::{cmp,eq,cmp_arith}` order a non-term **after every valid term** (never `Equal`; two non-terms by raw word), so `eq ⇒ equal hash` holds.
- **Pure tag math is `const fn`** where the body allows (raw-word construction, the tag tests, the `make_*`/`*val` accessors); files target ~1000 lines, one responsibility each.

## Design Decisions
- **`Eterm` = `#[repr(transparent)]` newtype over `usize`** (not a raw alias): type safety + methods + a documented ABI contract while staying layout/pass-identical to `ERL_NIF_TERM` (Part I §A.3).
- **Checked + unchecked constructors.** `make_small`/`make_arityval`/`make_pid_data` are unchecked (engine hot path, mirroring C); `try_*` variants return `TermResult` at trusted boundaries so range violations become errors, not silent corruption.
- **Fun descriptors inline the C `entry` pointer (an M1 substitution).** No fun/export table here, so the entry fields live *inside* the term; carrying every wire field keeps decode→encode byte-exact and lets the fun be called (External ⇒ module+function+arity; Imported ⇒ module+index).
- **`AtomTable::clone` mints a new *identity*, and the O(1) witness depends on it.** Append-only + monotonic generation + a length check proves "same table, only appended to" *given* a way to tell two tables apart, which a per-construction identity from a process-global counter supplies.
  Preserving identity across `clone` would be unsound under copy-on-write (`Rc::make_mut` clones the live table on intern), so a clone (and a `fork`) takes a fresh one; the only cost is a conservative memo rebuild, and the counter is bookkeeping, not observable behaviour.
- **`heap::Heap` is a minimal term-construction arena, NOT the production allocator** (that is `beam_alloc`); it exists so the representation has a testable backing store, and it never reallocates so interior pointers never dangle.
- **`TUPLE0` is `tuple0()`, a function, not a `const`** — a shared empty-tuple literal must carry `TAG_LITERAL_PTR` (a pointer to a static block), and Rust rejects pointer→integer casts in const eval. Same reason `make_boxed`/`make_list`/`make_tuple` are plain `fn`.
- **Off-heap term materialization lives here (`thing.rs`), heap-agnostic,** because this crate **cannot depend on `beam_alloc`** (a dependency sink; `beam_alloc` already depends on `beam_term`); `beam_codec` keeps only error-wrapping + allocation and forwards the word layout.
- **The bit/float payload readers live in `bitstring.rs`** (one public definition each); `compare.rs` and `hashing/` forward, `thing.rs` re-exports `float_bits`/`float_value` — avoiding `ambiguous_glob_reexports`.
- **The per-object layout walk has a single definition (`src/layout.rs`).**

## Test Strategy
- Per-module `#[cfg(test)] mod tests` in every `src/*.rs` pins that thing's **raw words**, write→read round-trips and typed rejections; `src/layout.rs` cross-checks `sum(object_words over the graph) == size_object`, `src/number.rs` pins hand-computed doubles/ties-to-even/the `bitlen` 1024-1025 boundary, and `hashing/` carries the structural-hash and oracle-vector suites.
- `tests/` owns the public-surface suites, the deterministic allocation pins and the live-`erl` cross-checks — per-file inventory and discipline in `tests/CONTEXT.md`.
  Two rules carry over: oracle suites **skip loudly, never fail**, when `erl` is absent (a dev-dependency on `beam_codec` would cycle), and **nothing asserts on wall-clock time** (this host is shared; allocation/word counts and retired instructions are the load-independent evidence).
- `tests/map_build_equivalence.rs` is an **equivalence gate, not a golden re-pin**: a dedup rewrite must keep the file unchanged and green.
- Exhaustive cross-checks against real ERTS belong to `../tools/differential` (the compatibility gate).
- Iteration: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_term` (never `cd`; the wrapper sets a writable `CARGO_HOME`). The workspace gate adds `--workspace`.

## Routing Table
Every `src/*.rs` file is a routing node; its item list is the module section above (the constant tables in the code are the authority).
- `src/lib.rs` → crate root: the module list, the flat re-exports and the `prelude` glob surface every dependent imports
- `src/eterm.rs` → the `Eterm` word newtype and its ABI contract
- `src/tag.rs` → tag constants/masks, `PrimaryTag`/`BoxedTag`, `primary_tag`, `is_header`, `is_term_word`
- `src/immediate.rs` → small ints, atom/NIL/catch/pid/port immediates, the checked `try_make_pid_data` seam
- `src/atom.rs` → `Atom`, `AtomTable` (interning, `atom.names` seeding, `hvalue`, `fork`, the O(1) witness)
- `src/list.rs` → cons-cell construction/access
- `src/boxed.rs` → boxed pointers, headers, arityval, kind predicates, map headers, the M1 internal reference
- `src/tuple.rs` → tuple construction/size/element access (`tuple0`)
- `src/heap.rs` → minimal bump term arena (test backing store, not the production allocator)
- `src/literal.rs` → `TAG_LITERAL_PTR` tagging, `tag_literal_area`/`set_literal_tag`, `LiteralArea`
- `src/ops.rs` → `TermClass`/`class`, numeric predicates, `THE_NON_VALUE`
- `src/compare.rs` → `cmp`/`eq`/`cmp_arith`/`cmp_flatmap_keys`, `class_rank`, `cmp_funs`, `fallback_cmp`, the local-node-aware entry points
- `src/compare/` → the layout-aware comparison arms (child node — `src/compare/CONTEXT.md`)
- `src/hashing/` → bit-exact `phash2`/`phash`, `LocalFunIdentity`, the internal structural hash (child node — `src/hashing/CONTEXT.md`)
- `src/ihash.rs` → bit-exact `erts_map_hash` + `swizzle_map_hash`/`hashmap_nibble`
- `src/layout.rs` → `object_words` + both child-slot enumerators (the single layout source of truth)
- `src/size.rs` → `size_object`/`size_object_bounded`, `SizingPolicy`, the scratch-backed entry points
- `src/size/` → the sizing walk's working memory and hasher (child node — `src/size/CONTEXT.md`)
- `src/thing.rs` → heap-agnostic float/bignum/flatmap/external-pid/port/ref blocks (the codec forwards here)
- `src/fun.rs` → the on-heap fun/export layout (`FunKind`, the three descriptors, the writers, `fun_local_key`)
- `src/map.rs` → the Erlang map (flat-map/HAMT builder, `collapse_eq_runs`, the readers, `map_words_capacity`)
- `src/record.rs` → native records (size fns, `write_record`, the `record_*` readers)
- `src/number.rs` → `big_to_f64` + `number_as_f64`
- `src/bitstring.rs` → `ErlHeapBits`/`ErlSubBits` writers+readers, `bitstring_parts`/`bit_at`, the float readers
- `src/refc.rs` → off-heap (`refc`) binaries (the `BinRef`+`ErlSubBits` pair, the on-heap limits)
- `src/error.rs` → `TermError`/`TermResult` + the `From<TermError> for BeamError` interop impl
- `tests/` → the public-surface suites, allocation pins and live-`erl` cross-checks (child node, 22 files — `tests/CONTEXT.md`)
- `examples/` → the measurement binaries (child node — `examples/CONTEXT.md`)

## Handoffs (owned by other nodes — escalate writes to the parent)
- **The M1 internal-reference seam** (layout above): a **local-node** reference decoded from the wire (`NEWER_REFERENCE_EXT`/`NEW_REFERENCE_EXT`) must be materialized with `boxed::write_internal_ref` and read with `boxed::internal_ref_id` (never `internal_ref_numbers`), while a reference for **another** node is `thing::write_external_ref`.
  The four words are byte-identical to `beam_process`'s `MonitorRef::to_eterm`, `beam_bifs`'s `make_ref_thing` and `beam_rt`'s native process floor (`tests/internal_ref.rs` pins the shape). Consumer owner: `beam_codec`.
- **An external fun arity > 255 and a zero-length external reference are both expressible.** `fun::write_fun_external` accepts any arity (true arity in descriptor word 3, read by `fun_true_arity`), which is what `beam_codec`'s `EXPORT_EXT` encoder emits; `thing::write_external_ref` accepts an **empty** `ids` slice.
- **A `Local` fun's `NEW_FUN_EXT` `old_index`/`old_uniq` are not in the term** (its descriptor is `[uniq:u64][index:u32]` by the frozen C layout), so the `beam_bifs::LocalFunIdentity` / `beam_codec::FunIdentity` seam supplies them from the defining module's `FunT` table (descriptor position ⇒ `old_index`, entry `uniq` ⇒ `old_uniq`); the residual gap is a fun whose defining module cannot supply them (the `-eval` `erl_eval` template closure).
- **`definition_hash` is the caller's to supply.** It is an explicit parameter of the heap-agnostic `write_record` because this crate cannot call `make_hash2`; `beam_codec::term::make_record` and `beam_engine`'s record family compute `hashing::compute_record_definition_hash` so `records:create/4`, `records:update/4` and `put_record`'s create form store the real hash.
  `beam_bifs::access::native_record_write` still passes a `make_small(0)` placeholder and is not on a runtime path (the compare suite pins the layout through it, where the stored word is irrelevant because `hashing` recomputes the hash).
- **A comparison that must agree with `erts_cmp_compound` needs the local node.** The adopter is `beam_bifs` (gated by `access::is_mixed_identity_pair`); `beam_process` never orders pid terms and `beam_ets`' `ordered_set` deliberately keeps `cmp_arith`. Detail: `src/compare/CONTEXT.md`.
- **`beam_message` prices its destination through `size::SizingPolicy`, never the bare `size_object*` forms** (the pitfalls are mutation-proven there). Detail: `src/size/CONTEXT.md`.
- **`immediate::try_make_pid_data` is the one pid-field bound.** `beam_bifs` takes it in both places that need it (`term_conv`'s `list_to_pid` reading, `bif.c:4582-4583`; `fun_info`'s `NEW_FUN_EXT` creator-pid check); two sites keep crate-local copies — `beam_codec`'s `decode/local_node.rs` (derived from `beam_term::immediate::PID_*_SIZE`) and `beam_process`'s `ProcessId::to_eterm` (an infallible `const fn` over the unchecked `make_pid_data`).

## Known Issues
- **Comparison gaps (`src/compare.rs`).** The classes without a faithful layout-aware arm use a **deterministic, panic-free `fallback_cmp`** and must be extended for exact ordering: on-heap bin refs (class rank, then object arity, then raw term word); internal references (raw payload words — for the M1 block those words *are* the identity, so equal id ⇒ `=:=` and ids order numerically); atoms whose name is absent from the passed `AtomTable` (raw index).
  Hash-map comparison cannot be byte-matched to ERTS (its HAMT order depends on its own hash), so the flat-map-key-order rule serves both shapes.
  Two documented conventions cover what the C asserts or cannot reach: a **same-node port** pair (the C raises `ERTS_INTERNAL_ERROR`) compares `internal_port_data` with the external 64-bit number, and same-node same-field pid/ref pairs fall through to `Equal` (the C's `CMP_NODES` no-op). `src/compare/CONTEXT.md` owns the field-by-field detail; `tests/compare_class_order.rs` pins both orders.
- **`Atom` identity is table-relative, not global.** `Atom` is a bare index into one `AtomTable`, so two atoms carry the same `Eterm` only if their table interned names in the same order; the `cmp_impl` fast path (`a.raw() == b.raw()` ⇒ `Equal`) then makes two atoms with the same index but different names compare `Eq`, and an out-of-range index falls back to raw-index comparison — silently breaking **ordering** only, since equality stays faithful.
  Name-faithful only when operands and the passed table share one table; a per-module `AtomTable` (as `beam_loader::Module` builds) makes cross-module atom comparison unreliable.
- **`term_hash` is internal, not compatibility-pinned** — it guarantees only `eq(a,b,atoms) ⇒ term_hash(a,atoms) == term_hash(b,atoms)`.
  `hash_words` mixes an external-ref block's header/arity while `cmp_external_refs` trims the longer id slice's leading **zero** ids, so two external refs of different id-array lengths whose extra ids are zero compare `Equal` yet hash differently — reachable only with a forged/foreign ref.
- **The HAMT is `erts_map_hash`-driven (`src/map.rs` + `src/ihash.rs`).** Atom keys are placed correctly only when the `AtomTable` matches the oracle's indices (seeding from `atom.names` gives parity); the C hash is itself non-portable across VM instances ("Limitation #1", it mixes pointers), so the pointer-derived classes use a documented deterministic substitute.
- **The map builder churns Rust memory per call:** `write_map` alone makes ~3.2 K `malloc`s / 373 KB at `n = 2 775` (a `Box`, a child `Vec` and a per-node slot `Vec` **per trie node**, plus the `entries`/`leaves` buffers), and `beam_engine`'s `put_map_assoc` adds two full `map_entries` pair `Vec`s (one built only for its `.len()`).

## Notes for Agents
- **Read `../../erts/emulator/beam/erl_term.h` before changing any constant** — it is the ground truth; cite it where a value is non-obvious.
- **No accessor validates provenance, liveness or alignment — by design.** `car`/`cdr` deref `list_val(x)` with no bounds/alignment check, `boxed_val` only masks `!TAG_PTR_MASK`, and the classifiers are pure word-bit tests (`is_list` = `(x & 0x2) == 0`, so it also accepts a `HEADER`-pattern word).
  A pointer into a freed, recycled or `Heap::reset()` region is therefore **indistinguishable** from a live one (`heap::Heap` exposes `hp`/`hend` and `LiteralArea` a per-instance `contains`, but no walker consults either and there is no global region registry — the production literal area is `beam_loader`'s), and `size`/`layout` reject only a bare `HEADER` word. Liveness gates belong to the consumer that owns the source region.
- **No fully safe dereferencing accessor layer.** Every accessor that reads a boxed pointer's header word is `pub unsafe fn` (the `car`/`cdr`/tuple accessors, the `is_*` kind predicates, the float/bit extent readers, `class`, the `thing.rs` `*_parts` readers, and the sibling-facing `map`/`bitstring`/`refc`/`record` entry points) — rustdoc is the authoritative list.
  Safe `fn`/`const fn` cover only raw-bit inspection (primary tag, immediates, `is_list`/`is_nil`, `is_boxed`/`is_header`, the `*_header` subtag tests, `header_subtag`/`map_header_*`/`arityval`, `boxed_val`/`list_val`/`tuple_val`) plus `cmp`/`eq`/`term_hash`/`size_object*`/`AtomTable` access, the map readers and `number::*`.
  A `#![deny(unsafe_code)]` consumer must deref `*boxed_val(t)` itself; a `*_header` subtag test takes a *header word*, not a boxed term.
- **The map interior layout *is* owned here** (`src/map.rs`) because `beam_gc` sizes it — keep it byte-compatible with the copier; every other interior layout not externally pinned (list chunking, ETS tree shape) is open and documented at its own node.
- **Lowering an allocation count here moves sibling exact-count pins** — they are deliberately exact, so a *reduction* fails them like a regression and the owner must re-derive the constant (`beam_codec/tests/alloc_count.rs`; `beam_bifs/tests/decode_hot_path.rs` and `beam_rt/tests/atom_table_commit_alloc.rs` use `AtomTable::clone` as a Θ(N) sensitivity control, so `Clone` must stay a deep copy).
- **No node-namespace classification or demotion lives here — an external pid/port/ref is never rewritten.** There is no "this node" notion outside the comparison: the mixed arms only *order* a pair against a caller-supplied `LocalNodeId`, and the `*_parts` readers return the stored sysname and creation verbatim, so **every consumer does its own identity check** (`beam_codec`'s `Decoder::is_local_node` does the `binary_to_term/1` demotion, `external.c:3124-3129`; `beam_rt` keeps its own for the native floor).
  The wire's `INTERNAL_LOCAL_SYSNAME` sentinel is `NIL` (`external.c:2965`), so an internal-format inbound pid may carry **`NIL` in word `1`** meaning "this node" — resolve it in the decoder, never by comparing words here.

## See Also
- `../../CONTEXT.md` — workspace contracts (§2 frozen term layout, §3 error strategy).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.1–A.3, §B; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A, §B.
- `../../erts/emulator/beam/CONTEXT.md` — legacy core; its "Term Encoding (Eterm) Reference" section.
- `../beam_abi/` — NIF/driver C ABI; transmutes `Eterm` ↔ `ERL_NIF_TERM`. `../beam_alloc/`, `../beam_gc/` — production heap/allocator + collector built on `heap.rs`.
- Child nodes: `src/compare/CONTEXT.md`, `src/hashing/CONTEXT.md`, `src/size/CONTEXT.md`, `tests/CONTEXT.md`, `examples/CONTEXT.md`.
