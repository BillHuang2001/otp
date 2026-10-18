# `beam_term` — Frozen BEAM Term Representation

## Intent
Owns the **frozen, bit-exact tagged term representation** of the Rust ERTS rewrite — the workspace's most load-bearing shared contract (workspace `CONTEXT.md` §2) — at **Level-1 drop-in fidelity** (a Rust core and the retained C periphery must not differ by one bit).
Ground truth: the legacy C (`../../erts/emulator/beam/erl_term.h`, `erl_term.c`).
Implements Part I §A.1–A.2 and Part II §B.
Every other crate depends downward on this one for term construction, inspection and classification; **no other crate may assume a different layout.**

## API Surface (FROZEN — every downstream crate routes here)
`Eterm` is a `#[repr(transparent)]` newtype over `usize`, ABI-identical to `ERL_NIF_TERM` (both one machine word); `beam_abi` transmutes between them.
No other crate may redefine it.
Only the frozen *constants*, *layouts* and *invariants* are stated here; each module (and the Routing Table) owns its item list, and every item is `pub` (or `pub unsafe fn` where the signature says so).

### `eterm.rs`
`pub struct Eterm(usize)` — `#[repr(transparent)]`, `Copy + Eq + Hash + Debug`, `Default` → `NIL`; `pub const fn raw`/`from_raw`/`nil`/`non_value`.

### `tag.rs`
The frozen C tag values: primary mask `0x3` with `HEADER=0x0`/`LIST=0x1`/`BOXED=0x2`/`IMMED1=0x3`, `TAG_PTR_MASK=0x7` (64-bit), `TAG_LITERAL_PTR=0x4`; IMMED1 (mask `0xF`) `PID=0x3`/`PORT=0x7`/`IMMED2=0xB`/`SMALL=0xF`; IMMED2 (mask `0x3F`) `ATOM=0xB`/`CATCH=0x1B`/`NIL=0x3B`; headers `HEADER_ARITY_OFFS=6`, `HEADER_SUBTAG_MASK=0x3C`, `MAX_ARITYVAL=(1<<24)-1`; `pub enum PrimaryTag`/`BoxedTag` (14 subtags, `0x00`…`0x38`).

### `immediate.rs`
Frozen widths: `SMALL_BITS=60` (`MAX_SMALL=(1<<59)-1`), `PID_NUM_SIZE=28`, `PID_DATA_SIZE=60`, `PID_SER_SIZE=32`, `PORT_DATA_SIZE=60`; `pub const NIL: Eterm` = `0x3B`.
- **`make_pid_data` is unchecked by design** — it mirrors the C macro (`erl_term.h:655-656`) exactly (`(ser << PID_NUM_SIZE) | num`, no masking, only `debug_assert`ed) because every C call site range-checks instead (`external.c:3176`, `bif.c:4582`); masking would *deviate*, since the C's `|` lets an overflowing number carry into the serial.
- `try_make_pid_data(ser, num) -> TermResult<u64>` is that call-site check as one seam, applied **before** the fields are combined; rejection is `TermError::PidIdOutOfRange` ⇒ `badarg` (the oracle's own answer at both C sites) against `MAX_INTERNAL_PID_DATA`/`MAX_INTERNAL_PID_NUMBER`/`MAX_INTERNAL_PID_SERIAL`.

### `atom.rs`
`pub struct Atom(u32)`; `ATOM_INDEX_BITS = 6`; `MAX_ATOM_INDEX = 2^58-1`; `Atom::MAX` = `u32::MAX`.
`AtomTable` assigns indices **in insertion order from 0**, one per distinct name, reproducing `atom.names`, and exposes `hvalue` plus the witness `generation`/`snapshot`/`appended_since` ([contract below](#atom-table-snapshots--the-o1-append-only-memo-witness)).
- `hvalue` is the C `atom_tab(a)->slot.bucket.hvalue` (Dragon-Book `hashpjw` over the UTF-8 name bytes + the latin1 clutch, `atom.c:82`) — the value `make_hash`/`make_hash2` mix for an atom; out-of-range falls back to `a.raw()`.
  The `atom.names` parser is a callback tokenizer, so seeding allocates one `Arc<str>` per name and no throw-away `Vec<String>`.
- **Backing store** `{ names: Vec<Arc<str>>, index: HashMap<Arc<str>, Atom>, instance: u64, generation: u64 }`: a name is stored **once** as an `Arc<str>` (`Arc`, not `Rc`, so the table stays `Send + Sync`), so `name` is **O(1)**, `intern` allocates **exactly one** `Arc<str>` for a novel name and **nothing** for a known one, and `clone()` is a deliberate Θ(N) deep copy with a fresh `instance` (Design Decisions) — never on a hot path.

### `boxed.rs`
Boxed pointers, headers, the arityval family (arity 0 encodes as the raw word `0`), the `MAP_HEADER_*` accessors, `make_record_header`, and macro-generated `unsafe` kind predicates on a *term* (`is_map`/`is_float`/`is_big`/`is_internal_ref`/`is_any_fun`/`is_record`/`is_bitstring`/`is_external_*`/`is_*_fun`) each with a `const fn` `*_header` twin taking a *header word*.
- **M1 internal-reference thing (the canonical layout):** frozen 4-word `[make_header(3, REF_SUBTAG), node, serial, id]`, `INTERNAL_REF_WORDS = 4`, written only by `unsafe write_internal_ref(hp, id)` and read back only by `unsafe internal_ref_id(t)` (`BadRef` for a non-term, another boxed kind, a C-layout arity-2 block, or a non-small id word).
  The whole identity rides that one 60-bit id word (`SmallIntOutOfRange` beyond `0..=2^59-1`) while ERTS's own id is a full 64-bit `Uint64` across three wire numbers; widening is a term-layout break, so `node`/`serial` stay zeroed and a bignum id would need a variable-size thing.
- `unsafe internal_ref_numbers<'a>(t) -> &'a [u32]` is the **C-layout reader** only; nothing calls it and it must **never** be fed an M1 reference (it would return small-int tags, not ids).

### `literal.rs` (off-heap literal areas, `TAG_LITERAL_PTR`)
`make_literal`, `is_literal` (bare bit test for `0x4`, meaningful **only** on BOXED/LIST words), `clear_literal`; `tag_literal_area(&mut [Eterm], &mut [Eterm])` is the **safe slice-based** form and `unsafe set_literal_tag` its raw-pointer shim (the `erts_set_literal_tag` graph walk); `LiteralArea` is the bump-allocated frozen block.
- The shared private `tag_words` walk steps over a HEADER body by `thing_arityval` — except a **fun** header, by `fun_desc_words(fun_kind_of_header(hdr))` (2/3/10), so the inlined raw descriptor words are never tagged, and `HEADER_SUB_BITS`, by `thing_arityval - 1` (the `orig` field).

### `compare.rs` (`erts_cmp`)
LCA ordering for `beam_ets` ordered_set, the `beam_bifs` comparison BIFs and `<`/`>`; `class_rank` fixes the cross-class ladder `small < big < float < atom < external_ref < ref < fun < external_port < port < external_pid < pid < tuple < record < map < nil < list < bitstring < bin_ref < non-value` (`rank = 0x10 - _DEF`, `erl_term.h:1441-1458`).
- `cmp(a, b, atoms)` is the exact (`=:=`) order (`erts_cmp(a,b,exact=1)`) with `eq` as its `.is_eq()`; `cmp_arith` is the arithmetic (`==`) order (`exact=0`, differing only for floats and mixed int/float pairs) using the C's **exact** mixed algorithm rather than an `f64` cast: `1 == 1.0`, `-0.0 == +0.0`, `9007199254740993 > 9007199254740992.0`, NaN/±inf deterministic.
  `cmp_flatmap_keys` is `erts_cmp_flatmap_keys` (`erl_utils.h:267-272`) — atoms by index, anything else by `cmp` — the order a flat map stores its keys in.
- **Same-class structural arms:** external pids, ports, references and native records compare field-by-field in the C's order, so a **copy is `=:=` its original** — pid `serial → number → node`, port `node → number`, ref `node → ids` (as a bignum with the highest-index zero ids trimmed), record `module → name → is_exported(false<true) → field count → keys → values`.
- **Local-node-aware entry points** (the mixed internal/external arms, `src/compare/identity.rs`): `LocalNodeId { sysname: Eterm (an atom), creation: u32 }` is the runtime's own node (`erts_this_node`); `cmp_with_local_node(a, b, local, atoms)` (exact) and `cmp_arith_with_local_node` (arithmetic) take it, and `cmp`/`cmp_arith` are the `local: None` forms.
  One internal `CmpCtx { atoms, exact, local }` is threaded through `cmp_impl` and every recursive arm; `atoms` is still a parameter because node order is **name**-based (`erts_cmp_atoms`), which no hash reproduces.
  With a `local`, a mixed internal/external pair of the *same* category is decided **before** the rank shortcut, field by field: pid `serial → number → node`; port **node only** (the C never compares port numbers; a same-node pair — unreachable in the C, which asserts — compares the numbers as a documented convention); ref `node → ids`, the M1 internal id expressed as the id array `[id as u32, (id >> 32) as u32]` and shared with the external arm through `cmp_ref_ids` (trim the longer side's leading high-index zeros, then compare high-index down). Same node + same fields ⇒ `Equal` (the C's `CMP_NODES` no-op then `pop_next`).
  With `local: None` the mixed pair stays **rank**-ordered (`external_*` below `*`), the documented no-context convention; the new entry points are byte-identical to `cmp`/`cmp_arith` for every other pair.
- **Maps (`src/compare/maps.rs`):** two flat maps whose keys are non-descending in `cmp_flatmap_keys` order compare straight out of storage with **no allocation**; any other pair (a hash map, or a flat map an order-preserving producer built unsorted) uses the collecting `map_pairs_flatmap_order` fallback over the same key-then-value sequence.
  `cmp_maps` takes the `CmpCtx` for its **values**; its **keys** stay on the canonical `cmp_flatmap_keys` order (a map's stored key order must not depend on which node asks), so the fast path and the fallback still compare one rule.
### `compare/numbers.rs`
`pub(super) cmp_numbers(...)` — the `mixed_types` port of `erts_cmp_compound` plus `erts_float_comp` and the private `double_to_big`: exact comparison for a pair of *different* numeric classes (small↔float inside the lossless `±(2^53−2)` window else exact integer subtraction; big↔float by magnitude bands, lossless `big_to_f64`, then exact `double_to_big` + bignum).

### `hashing/` — `phash2`/`phash` (pinned) + internal structural hash
`mod.rs` (docs + `term_hash` + tests), `common.rs` (the `pub(super)` ERTS-hash primitives), `make_hash2.rs`, `make_hash.rs`.
- **Pinned contract:** `make_hash2(t, atoms)` and `make_hash(t, atoms)` (`-> TermResult<u32>`) are bit-exact u32-wrapping ports of `erl_term_hashing.c` — the values `erlang:phash2/1,2` and `erlang:phash/2` return — and unlike `term_hash` **these ARE part of the Level-1 drop-in contract** (oracle-cross-checked); they cover every class this crate can represent, after the C's `hash == 0` fast paths.
- `compute_record_definition_hash(def, atoms)` is the synthetic definition hash the C caches — named `compute_*` to avoid colliding with the header-*reader* `record::record_definition_hash`; both ports mix a record's **real computed** definition hash, never the placeholder `write_record` stores, and walk the values in sorted-key order.
- **Identity gaps (never a wrong value):** `FunKind::Local` ⇒ `TermError::FunIdentityUnavailable` (its 2-word descriptor carries no module/`old_uniq`); an **internal reference** ⇒ `TermError::RefIdentityUnavailable` (the C mixes a node-container id this layout does not carry).
- **Internal structural hash:** `term_hash(a, atoms) -> u64` — deterministic, `eq(a,b,atoms) ⇒ equal hashes`; **its exact values are internal, NOT part of any compatibility contract.**

### `ihash.rs` — bit-exact `erts_map_hash`
The Erlang map hash (`erl_term_hashing.c:1955-2130`) plus `swizzle_map_hash` (nibble reversal) and `hashmap_nibble` (the level-`L` least-significant nibble); it drives `map.rs` leaf placement so a `write_map` hash map's stored pair order equals the oracle's `maps:to_list/1` order.
Pointer-derived classes use a documented deterministic substitute (the C hash mixes pointers).

### `layout.rs` — canonical per-object layout (single source of truth)
How many heap words an object occupies and which payload words a copier must forward; `size.rs` consumes it, `beam_message::layout` re-exports the walk, and `beam_gc::copy::move_size` is `object_words` plus the copy-specific `ErlSubBits` override.
- `object_words(header, ptr)` (header included): tuple/record `arity + 1`; fun `1 + fun_desc_words(kind) + fun_num_free`; flatmap `size + 3`; HAMT node `1 + header_arity + popcount(MAP_HEADER_VAL)`; other things `thing_arityval + 1`; `ErlSubBits` → `ERL_REFC_BITS_SIZE` (8) when `orig` is a `BinRef`, `heap_bits_size_words(end - start)` for a match context, else `ERL_SUB_BITS_SIZE` (5).
  `tuple0()` is the caller's special case.
- `for_each_sizing_child_slot` and `for_each_child_slot` differ **only** for `ErlSubBits`: the sizer gets no child (descending into `orig` would double-count), the copier gets its `orig` word (`SUB_BITS_ORIG_WORD` = 4) because the term must be forwarded (`copy.c:219-242` vs `:1677-1700`).

### `size.rs`
`size_object(a)` — flat heap words the graph occupies: `0` immediates, `2` per cons cell, `arity + 1` per boxed object, plus nested sizes; **bounded**, so a cyclic/forged graph gives a defined partial count.
`size_object_bounded(a, limit)` — `Ok(n)` iff acyclic and `n <= limit`, `Err(TermGraphTooLarge)` over limit, `Err(TermGraphCyclic)` for a cycle (priority over the limit); one explicit-stack, **path-based** DFS over `layout`'s interior, so a shared-but-acyclic subterm counts once per occurrence and a non-term word contributes `0` and is never dereferenced.
`SizingPolicy` (`DefaultSizingPolicy` = both defaults) — the caller-supplied half of the C `size_object_x(t, litopt)` (`global.h:1135/1136`), with exactly two independent, object-safe axes: `is_shared(t)` prices a subterm the consumer reuses **by identity** at `0` words and never descends into it (the `litopt` literal skip, `copy.c:99`/`:119`), and `object_words(header, payload) -> Option<usize>` overrides one boxed object's own words (`None` = prices nothing and exposes no child; default = `layout::object_words`), which is what lets a copier price a refc binary it must flatten as its window `heap_bits_size_words(end - start)` instead of the 8-word pair.
`size_object_with(a, &dyn SizingPolicy)` / `size_object_bounded_with(a, limit, &dyn SizingPolicy)` — the parameterised walks; `size_object`/`size_object_bounded` are exactly the `&DefaultSizingPolicy` case, so the seam is unobservable to them.
A policy changes *pricing* only: child slots always stay `layout::for_each_sizing_child_slot`'s answer, and the shared zero-arity tuple literal is `0` words under every policy (the C's `arity == 0` guard sits outside the `litopt` block).

### `thing.rs` — off-heap term materialization (heap-agnostic)
Writers/readers for the boxed "thing" families this crate owns (float, bignum, flat map, external pid/port/ref — `ExternalThing`), byte-identical to the oracle-verified `beam_codec::term`, which forwards here; every writer takes a caller-allocated `hp: *mut Eterm` (`beam_alloc` in production, `heap::Heap` in tests) and returns the tagged root.
- Float `FLOAT_SIZE_WORDS = 2`; bignum `big_size_words(n)` = `n + 1`, one `ErtsDigit`=`u64` per word, LS-first, sign in `_BIG_SIGN_BIT`; flat map `flatmap_size_words(n)` = `3` empty else `4 + 2n`, block `[arityval(n), k0..k(n-1), MAP_HEADER_FLATMAP, n, keys-tuple, v0..v(n-1)]`, root `make_boxed(hp + 1 + n)`.
- External things (incl. the M1 "`next` holds `creation`" substitution): `MAX_REF_IDS = 5`; pid/port blocks are 4 words, `external_ref_size_words(n_ids)` = `3 + n_ids/2 + 1` on `ARCH_64`. **`ids` may be empty**: a zero-id external reference round-trips, block `[header, node, creation, count = 0]`, `external_ref_size_words(0) == 4`, hashing mixes the zeroed word after count ⇒ `0`.

### `bitstring.rs` — bit-level payloads
`ErlHeapBits`/`ErlSubBits` writers+readers plus the public bit/float payload helpers; bits are **MSB-first inside each byte** and the last byte's unused trailing bits are zeroed; `ERL_SUB_BITS_FLAG_MASK = 0x3` (`_MUTABLE`/`_VOLATILE`).
- `bit_count_to_bytes(n_bits)` = `ceil(n_bits/8)`, `heap_bits_size_words(n_bits)` = `2 + ceil(ceil(n_bits/8)/8)`; `write_heap_bits` builds `[make_header(words-1, HEAP_BITS_SUBTAG), n_bits, data…]`, rejecting (with `BadBitstring`) a `bytes` shorter than `ceil(n_bits/8)`; `write_binary`/`write_sub_bits`/`sub_bits_parts` cover the 5-word `ErlSubBits` block.
- `bitstring_parts` returns (base, **bit** offset, **bit** size); `bit_at`/`float_bits`/`float_value` live here as the crate's single float readers.

### `refc.rs` — off-heap (`refc`) binaries
The on-heap `BinRef` + `ErlSubBits` pair for a binary/bitstring above the on-heap limits: the 8-word arrangement is `[BinRef(3)][ErlSubBits(5)]` with the tagged root at `hp + 3`; `ERL_REFC_BITS_SIZE = 8`, `ERL_ONHEAP_BINARY_LIMIT = 64`, `ERL_ONHEAP_BITS_LIMIT = 512`; `refc_offset`/`refc_size` report **bits**.
- **Frozen inter-crate contract (`beam_alloc`/`beam_gc`):** `BinRef.val` = raw pointer to the payload's **first data byte** (the off-heap header is `beam_alloc`'s concern); `ErlSubBits.orig` is a term (a boxed `BinRef`, or an on-heap bitstring for a match context); the 8 words move/copy together with `orig` fixed up to the new `BinRef`.

### `map.rs` — flat map + HAMT/CHAMP builder, lookups, size
The Erlang map is a boxed `MAP_SUBTAG` term: flat (`MAP_HEADER_FLATMAP`, written by `thing.rs`) for ≤ `MAP_SMALL_MAP_LIMIT = 32` pairs, else a **hash map** — a 16-way (`4` hash bits/level, `HAMT_MAX_LEVEL = 16`) CHAMP.
- Trie node layouts (frozen; what `beam_gc::copy::move_size` sizes): head-array `[hdr][size][16 slots]` (18 words), head-bitmap `[hdr(bmp)][size][n slots]` (`2+n`), inner bitmap `[hdr(bmp)][n slots]` (`1+n`), collision node = a `make_arityval(n)` tuple of `n` key/value cons cells; one **term per slot**.
  Level `L` selects the **`L`-th least-significant** nibble of the key's `erts_map_hash` (level 0 = LSB nibble).
- `write_map(hp, pairs, atoms)` dedups (last wins) and picks the layout from the **input** arity: ≤ `MAP_SMALL_MAP_LIMIT` input pairs is necessarily flat (dedup can only shrink), so they are `eq`-deduplicated into ONE buffer and sorted by `cmp_flatmap_keys` with no `erts_map_hash` call; above the limit it builds the hash map keyed by `erts_map_hash`, leaves ascending by `swizzle_map_hash(hash)` (the C's `hxnodecmp` order) with equal-hash runs broken by `cmp` on the key, so the stored pair order equals the oracle's `maps:to_list/1` (insertion-order free).
- Readers `map_size`/`map_get`/`map_pairs` (sorted by `compare::cmp`)/`map_pairs_storage_order` (the *stored* order an image-preserving serializer needs) plus the functional `map_put`/`map_remove`; a non-boxed/non-map term ⇒ `TermError::BadMap`.

### `fun.rs` — the on-heap fun / export term layout
A `make_fun3` local closure or an `EXPORT_EXT`/`NEW_FUN_EXT` literal (`erl_term.h:403-425`, `erl_fun.h:61-99`, `external.c:3966-4003/5006-5040/5102-5176`, `utils.c:2149-2210`); `pub enum FunKind { Local, External, Imported }`.
- **Header word (C-exact):** bits 0..5 `FUN_SUBTAG` (`0x14`), 8..15 `arity`, 16..23 `num_free`, bit 24 `FUN_HEADER_KIND_OFFS` (C external), bit 25 `FUN_HEADER_IMPORTED_OFFS` (this crate's `NEW_FUN_EXT` marker, valid only while bit 24 is clear, so a C reader's `is_external_fun` still sees a local fun); `FUN_DESC_WORDS_{LOCAL=2,EXTERNAL=3,IMPORTED=10}`.
- **Word layout:** `env[i]` at `hp[1 + desc_words(kind) + i]`, block = `1 + desc_words + num_free`; `Local` = `[hdr][uniq:u64][index:u32][env…]`; `External` = `[hdr][module][function][arity raw]`; `Imported` = `[hdr][module][uniq[0..8]][uniq[8..16]][index][old_index][old_uniq][pid_node][pid_number][pid_serial][pid_creation][env…]`.
- **A fun's true arity is out of band.** The header's 8-bit `arity` is what `fun_info(F, arity)` reports, but the C encoder emits the `Export`'s full arity, so the `External` descriptor is `[hdr][module][function][true arity]` and `write_fun_external` accepts **any** arity (masking the header, `num_free == 0`); oracle-verified: arity 256 reports `{arity,0}` yet re-encodes 256.
  `fun_true_arity` is what an encoder/`compare`/hashing must use; wrong kind or non-fun ⇒ `TermError::BadFun`.

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

### `error.rs`
`pub enum TermError` / `pub type TermResult<T>`, reconciled into `beam_common::BeamError` by an owned `From` impl (`?`-interop for every dependent): `SmallIntOutOfRange` ⇒ `BadArith`; `ArityOutOfRange`/`AtomIndexOutOfRange` ⇒ `SystemLimit`; `HeapExhausted` ⇒ `Alloc(..)`; everything else ⇒ `BadArg`.
- `PidIdOutOfRange` — `try_make_pid_data` rejected an internal pid's process number or serial ⇒ `BadArg`, the oracle's own answer at both C call sites.
  `BadMap`/`BadBitstring`/`BadFun`/`BadRecord`/`InvalidAtomName` are named for the family that rejects them; `BadRef` is a reference accessor applied to a non-M1-reference thing (distinct from `RefIdentityUnavailable` and from `NotATerm`).
- Capability gaps that must never become a wrong value: `FunIdentityUnavailable`/`RefIdentityUnavailable` (`make_hash`/`make_hash2` only — `term_hash` is unaffected); `TermGraphCyclic`/`TermGraphTooLarge` come from `size_object_bounded`.

## Atom-table snapshots — the O(1) append-only memo witness
`AtomSnapshot` + `AtomTable::{snapshot, appended_since, generation}` let a caller validate a memoized *image* of the table in **O(1)** (`src/atom.rs`; measured by `examples/atom_witness.rs`).
- **Contract.** `AtomTable` is append-only (`intern` its only mutating path), so an index — and any prefix — is stable for the table's lifetime.
  `appended_since(snapshot)` is `true` **only** when `self` is the **same instance** as the snapshotting table *and* the length delta equals the generation delta (one name per generation bump); it never scans, never allocates, never returns a wrong `true`.
  Every index `< snapshot.len()` still names the same atom; names added since occupy indices `>= snapshot.len()`.
  `AtomSnapshot` exposes `len()`/`is_empty()`/`generation()`.
- **Memo protocol (both halves required).** Reuse `image` as an exact index→atom image of `shared` iff `shared.appended_since(snap) && image.len() <= shared.len()`, then extend with `shared`'s names at `image.len()..shared.len()`, re-taking `snap = shared.snapshot()` on every extension, and drop the memo if a decode fails part-way.
- **Adoption (outside this node).** `beam_bifs::binconv`'s per-thread `DECODE_SCRATCH`/`with_decode_scratch` validates its memoized table image exactly this way (O(1) plus the appended delta, no atom name read), with the O(table) `prefix_matches` surviving only as the fallback for a table the witness cannot vouch for.
  In `beam_rt`, `CodeServer::atoms_mut` is `Rc::make_mut`, so a novel-atom intern mints a fresh instance and conservatively invalidates the memo (steady state does no `make_mut`, so the witness holds).

## Constraints
- **Bit-exact layout (Level-1 drop-in).** Every constant, mask, shift and encoding must match `../../erts/emulator/beam/erl_term.h` exactly.
- **Dependencies:** `beam_common` + `libc` only (foundation dependency sink).
  `beam_common` is used for exactly one thing — the `From<TermError> for beam_common::BeamError` impl in `error.rs`, owned here so every dependent gets `?`-interop for free; `TermError`/`TermResult` must not be redefined per dependent.
- **`unsafe` confinement:** `heap`, `literal`, `list`, `boxed`, `tuple`, `thing`, `fun`, `map`, `record`, `compare`, `hashing`, `ihash`, `bitstring`, `refc`, `number`.
  Every `unsafe fn` documents `# Safety`; every `unsafe` block carries `// SAFETY:`; `unsafe_op_in_unsafe_fn` stays clean.
- **A word that is not a term is never dereferenced.** `THE_NON_VALUE` (release `0`; debug `make_header(0, FLOAT_SUBTAG)`) has primary tag `HEADER`, which the C `is_boxed` bit trick *accepts*, so a bare-`is_boxed`-guarded deref could touch garbage.
  The guards are `tag::is_term_word(x) = !is_header(x)` and `boxed::is_boxed_term(x) = is_boxed(x) && is_term_word(x)` — **every deref/classification site tests one of them first**.
  Non-term policy: fallible APIs ⇒ `TermError::NotATerm` (`make_hash`/`make_hash2`); total APIs give a defined answer (`ops::class` ⇒ `NonValue`, `size_object` ⇒ `0`, `number_as_f64` ⇒ `NaN`, `term_hash` ⇒ a tagged raw-word hash); `compare::{cmp,eq,cmp_arith}` order a non-term **after every valid term** (never `Equal`; two non-terms by raw word), so `eq ⇒ equal hash` holds.
  The `pub unsafe fn *_parts` readers and `heap.rs` keep their "caller passes a live thing of this kind" contract.
- **Pure tag math is `const fn`** where the body allows (`raw`, `from_raw`, `primary_tag`, tag tests, `make_small`, `signed_value`, `make_atom`, `atom_val`, `make_arityval`, `arityval`, …).
- Files target ~1000 lines; one responsibility per file.

## Design Decisions
- **`Eterm` = `#[repr(transparent)]` newtype over `usize`** (not a raw alias): type safety + methods + a documented ABI contract while staying layout/pass-identical to `ERL_NIF_TERM` (Part I §A.3).
- **Fun descriptors inline the C `entry` pointer (an M1 substitution).** No fun/export table here, so the entry fields live *inside* the term; carrying every wire field keeps decode→encode byte-exact and lets the fun be called (External ⇒ module+function+arity; Imported ⇒ module+index).
- **Checked + unchecked constructors.** `make_small`/`make_arityval`/`make_pid_data` are unchecked (engine hot path, mirroring C); `try_*` variants return `TermResult` at trusted boundaries so range violations become errors, not silent corruption.
- **`Atom` is `u32`.** The representation allows 58-bit indices; the practical table is capped far below 2^32 (default `+t` limit 1,048,576).
  Atoms only come from `make_atom(Atom)`, so the encoding invariant holds.
- **`AtomTable` assigns indices in insertion order (0-based)**, reproducing `atom.names` (`../../erts/emulator/utils/make_tables`: `#define am_<alias> make_atom($i)`), which preloaded modules that reference atoms positionally require.
- **`AtomTable::clone` mints a new *identity*, and the O(1) witness depends on it.** Append-only + monotonic generation + a length check proves "same table, only appended to" *given* a way to tell two tables apart, which a per-construction identity from a process-global counter supplies.
  Preserving identity across `clone` would be unsound under copy-on-write (`Rc::make_mut` clones the live table on intern), so a clone takes a fresh one; the only cost is a conservative memo rebuild, and the counter is bookkeeping, not observable behaviour.
- **`AtomTable::clone` is deliberately a deep copy of the name storage** (a fresh `Arc<str>` per entry): sharing the source's `Arc`s would make a clone Θ(1) in allocations and silently hollow out the clone-as-sensitivity-control in `beam_rt/tests/atom_table_commit_alloc.rs` and `beam_bifs/tests/decode_hot_path.rs`.
  `Arc` (not `Rc`) preserves `Send + Sync`.
- **`heap::Heap` is a minimal term-construction arena, NOT the production allocator** (that is `beam_alloc`); it exists so the representation has a testable backing store, and it never reallocates so interior pointers never dangle.
- **`TUPLE0` is `tuple0()`, a function, not a `const`** — a shared empty-tuple literal must carry `TAG_LITERAL_PTR` (a pointer to a static block), and Rust rejects pointer→integer casts in const eval.
  Same reason `make_boxed`/`make_list`/`make_tuple` are plain `fn`.
- **Off-heap term materialization lives here (`thing.rs`), heap-agnostic.** This crate **cannot depend on `beam_alloc`** (dependency sink; `beam_alloc` already depends on `beam_term`), which is why every writer takes a caller-allocated `hp` and returns the tagged root; `beam_codec` keeps only error-wrapping + allocation and forwards the word layout.
- **The bit/float payload readers live in `bitstring.rs`** (one public definition each); `compare.rs` and `hashing/` forward, `thing.rs` re-exports `float_bits`/`float_value` — avoiding `ambiguous_glob_reexports`.
- **The per-object layout walk has a single definition (`src/layout.rs`).** The sizing and copy child enumerations are deliberately split because they diverge for `ErlSubBits` (sizing must NOT descend into `orig` — it would double-count; a copier MUST forward it).
- **`cmp_arith`'s mixed integer/float compare is exact (`src/compare/numbers.rs`)** — ports the C `mixed_types` block instead of rounding to `f64`; `double_to_big` is a private single definition there.

## Test Strategy
- Per-module `#[cfg(test)] mod tests` in every `src/*.rs` pins that thing's **raw words** (the layouts above; `HEADER_BIN_REF = 0xA8`, `HEADER_SUB_BITS = 0x124`, the 8-word `[BinRef][ErlSubBits]` pair rooted at `hp + 3`, the `MAX_REF_IDS`/single-digit/empty-flatmap/negative-bignum boundaries) plus write→read round-trips and typed rejections.
  `src/layout.rs` cross-checks `sum(object_words over the graph) == size_object`; `src/number.rs` pins hand-computed doubles, ties-to-even and the `bitlen` 1024/1025 boundary; `src/hashing/` carries the structural-hash + oracle-vector suites.
- Exact-raw-pattern and policy suites: `tests/layout.rs` (`make_small(0) == 0xF`, `make_atom(0) == 0xB`, `NIL == 0x3B`, `make_arityval(1) == 0x40`, cons tag `0x1`, boundary sweeps), `tests/abi.rs` (`size_of`/`align_of` = `usize`, `Eterm: Copy`, `repr(transparent)`), `tests/non_term.rs` (the non-term policy over the public API).
- Shape/limit suites: `tests/atom_witness.rs` (a counting `#[global_allocator]` proves `snapshot`/`appended_since` are allocation-free; a 4-vs-200 000-atom timing bound, plus clone-divergence/replaced-table/shrunken-table soundness at 50 000 atoms), `tests/size_bounded.rs` (cycles, budget inclusivity, non-terms ⇒ `Ok(0)`, a 100 000-element list, exact flat/HAMT sizes, per-occurrence counting), `tests/internal_ref.rs` (the M1 layout pinned literally — raw header `0xD0` — plus round-trip, typed errors, 4-word size, `=:=`/ordering), `tests/record_ref_hash.rs` (offline `(make_hash2, make_hash, definition_hash)` pins + a real `-record` fixture cross-check).
- **Comparison suites:** `tests/compare_terms.rs` (ordering across class pairs), `tests/compare_external.rs` (the external/record arms with a live-`erl` check), and `tests/compare_class_order.rs` — the **full** 18-rung cross-class ladder (every adjacency and the whole `i<j ⇒ Less`/`i==j ⇒ Equal`/`i>j ⇒ Greater` matrix), the rank-order-vs-numeric-rung split between `cmp` and `cmp_arith`, every fun kind on the fun rung, a refc binary on the bitstring rung, the six mixed internal/external boundary pairs, plus a live-`erl` oracle matrix.
- Counting `#[global_allocator]` pins: `tests/intern_alloc.rs` (one novel `AtomTable` intern allocates one `Arc<str>`; `lookup`/`name`/a known-name `intern` allocate 0; `seed` of 3000 names ≤ 3008; `clone` is Θ(table) — the sensitivity the sibling clone controls need) and `tests/map_alloc.rs` (`cmp` of two flat maps allocates **0**; `write_map` of a 32-pair input allocates **1** and is word-for-word `write_flatmap`'s output; a >32-key pair is the sensitive control; a hand-built *unsorted* flat map still compares exactly).
- Live-`erl` oracle cross-checks (**skip loudly, never fail**, when `erl` is absent; text protocol, because a dev-dependency on `beam_codec` would cycle): `tests/oracle.rs` (map size/get/pairs at the 31/32/33/40/100-key flat/hash boundary; bitstring parts/bits; bignum `eq`/`cmp`/`cmp_arith`/`size_object` across the 2-/3-limb boundary; flat-map stored key order vs `maps:to_list/1`), `tests/map_hash.rs` (HAMT pair-storage order: two offline-pinned vectors + 33/40/50/64/100-key mixed maps), `tests/number_oracle.rs` (`big_to_f64` vs `float/1` as IEEE-754 bits over a bit-length sweep + randomised limbs; `ERL_TIMEOUT = 60s`), `tests/compare_oracle.rs` (arithmetic/exact comparison vs `==`/`=:=`/`<`/`>` incl. the `>2^53` asymmetry).
- Exhaustive cross-checks against real ERTS belong to `tools/differential` (the compatibility gate).
  Iteration: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_term`.
  Never `cd`.

## Routing Table
- `src/tag.rs` → tag constants/masks/shifts, `PrimaryTag`/`BoxedTag`, `primary_tag`, `is_header`, `is_term_word`
- `src/eterm.rs` → the `Eterm` word newtype, ABI contract
- `src/immediate.rs` → small ints, atom immediates, NIL, catch, pid/port immediates, the checked `try_make_pid_data` seam
- `src/atom.rs` → `Atom`, `AtomTable` (interning, `atom.names` seeding/parser, `hvalue`, the O(1) append-only witness)
- `src/list.rs` → cons-cell construction/access
- `src/boxed.rs` → boxed pointers, headers, arityval, sub-type predicates, map headers, the M1 internal-reference thing (`write_internal_ref`/`internal_ref_id`/`INTERNAL_REF_*`) and the C-layout reader `internal_ref_numbers`
- `src/tuple.rs` → tuple construction/size/element access (`tuple0`)
- `src/heap.rs` → minimal bump term arena
- `src/literal.rs` → `TAG_LITERAL_PTR` tagging + `tag_literal_area`/`set_literal_tag`/`LiteralArea`
- `src/ops.rs` → `TermClass`/`class`, numeric predicates, `THE_NON_VALUE`
- `src/compare.rs` → `cmp`/`eq`/`cmp_arith`/`cmp_flatmap_keys` + the local-node-aware `cmp_with_local_node`/`cmp_arith_with_local_node` (the `erts_cmp` port, `class_rank`, deterministic fallbacks, the internal `CmpCtx { atoms, exact, local }` threaded through `cmp_impl`); `src/compare/identity.rs` → `LocalNodeId` + `cmp_mixed` (the internal-vs-external pid/port/ref field arms) + `cmp_ref_ids` (the trim-leading-zeros big-integer id compare shared with the external arm); `src/compare/numbers.rs` → exact `mixed_types` compare + private `double_to_big`; `src/compare/maps.rs` → the `MAP_SUBTAG` arm (`cmp_maps`, the allocation-free flat-vs-flat fast path over `thing::flatmap_parts` + the collecting fallback)
- `src/hashing/mod.rs` → internal structural hash `term_hash`; `src/hashing/common.rs` → shared ERTS-hash primitives; `src/hashing/make_hash2.rs` → bit-exact `make_hash2` + `compute_record_definition_hash`; `src/hashing/make_hash.rs` → bit-exact `make_hash` + `hash_binary_bytes`
- `src/ihash.rs` → bit-exact `erts_map_hash`/`erts_internal_hash`/`erts_internal_salted_hash` + `swizzle_map_hash`/`hashmap_nibble`
- `src/size.rs` → `size_object` + bounded, cycle-safe `size_object_bounded`
- `src/layout.rs` → `object_words` + `for_each_sizing_child_slot`/`for_each_child_slot` (the single layout source of truth)
- `src/thing.rs` → heap-agnostic float/bignum/flatmap/external-pid/port/ref blocks (`ExternalThing`; the codec forwards here)
- `src/fun.rs` → the on-heap fun/export layout (`FunKind`/`FunPid`, `make_fun_header`/`fun_kind`/`fun_words`/`fun_block_words`, `write_fun_{local,external,imported}`, the `fun_*` accessors)
- `src/map.rs` → the Erlang map (flat-map/HAMT builder `write_map`, readers/lookups, `MAP_SMALL_MAP_LIMIT`, `make_map_header`/`map_words_capacity`)
- `src/record.rs` → native records (size fns, `write_record`, the `record_*`/`record_definition_*` readers)
- `src/number.rs` → `big_to_f64` + `number_as_f64`
- `src/bitstring.rs` → `ErlHeapBits`/`ErlSubBits` writers+readers, `bit_count_to_bytes`/`heap_bits_size_words`, `bitstring_parts`/`bit_at`, `float_bits`/`float_value`
- `src/refc.rs` → off-heap (`refc`) binaries (`BinRef`+`ErlSubBits` pair writers/accessors, `ERL_REFC_BITS_SIZE`/`ERL_ONHEAP_*`, `is_refc_binary`/`refc_*`)
- `src/error.rs` → `TermError`/`TermResult` + the `From<TermError> for beam_common::BeamError` interop impl
- `tests/` → crate-level integration tests (`layout.rs`, `abi.rs`, `non_term.rs`, `compare_terms.rs`, `compare_external.rs`, `compare_class_order.rs`, `size_bounded.rs`, `internal_ref.rs`, `atom_witness.rs`, `intern_alloc.rs`, `map_alloc.rs`, `record_ref_hash.rs`, and the live-`erl` cross-checks `oracle.rs`/`map_hash.rs`/`number_oracle.rs`/`compare_oracle.rs`)
- `examples/atom_witness.rs` → witness micro-benchmark (`run --release --example atom_witness`)

## Handoffs (owned by other nodes — escalate writes to the parent)
- **The M1 internal-reference seam.** A **local-node** reference decoded from the wire (`NEWER_REFERENCE_EXT`/`NEW_REFERENCE_EXT`) must be materialized by reserving `INTERNAL_REF_WORDS` words and calling `boxed::write_internal_ref` — never by hand-writing the words — and read back with `boxed::internal_ref_id` (never `internal_ref_numbers`, which describes the C layout only).
  A reference for **another** node is an external reference (`thing::write_external_ref`); the wire form's local node identity is not stored in the M1 block (`node`/`serial` are zeroed).
  **Parity:** the four words are byte-identical to `beam_process`'s `MonitorRef::to_eterm`, `beam_bifs`'s `make_ref_thing` and `beam_rt`'s native process floor; `tests/internal_ref.rs` pins that shape literally.
  Consumer owner: `beam_codec`.
- **`beam_codec` can carry an external fun arity > 255 and a zero-length external reference.** `fun::write_fun_external` accepts any arity (true arity in descriptor word 3, read by `fun_true_arity`), so an encoder should emit `EXPORT_EXT`'s `info.mfa.arity` through it and reserve `fun_words(External, 0)`; `thing::write_external_ref` accepts an **empty** `ids` slice.
- **A `Local` fun's `NEW_FUN_EXT` `old_index`/`old_uniq` are not in the term** (its descriptor is `[uniq:u64][index:u32]` by the frozen C layout), so the `beam_bifs::LocalFunIdentity` / `beam_codec::FunIdentity` seam must supply them from the defining module's `FunT` table (descriptor position ⇒ `old_index`, entry `uniq` ⇒ `old_uniq`); the residual gap is a fun whose defining module cannot supply them (the `-eval` `erl_eval` template closure).
- **Record / number follow-ups.** `beam_bifs` (`is_record/2,3`, `records:*`), `beam_codec` (`RECORD_EXT`, tag 67) and `beam_engine` (`Fconv`) can consume `record::*`, `number::big_to_f64` and `hashing::compute_record_definition_hash`; the last lets `records:create/4`/`update/4` compute the definition's `hash` word (`erl_record.c:1041-1046`) instead of the `make_small(0)` placeholder `write_record` is currently handed.
- **Term comparisons that must agree with `erts_cmp_compound` need the local node.** `cmp_with_local_node(a, b, local, atoms)` (exact, `=:=`/ordering) and `cmp_arith_with_local_node` (arithmetic, `==`) take a `compare::LocalNodeId::new(sysname_atom, creation)` describing `erts_this_node`; `cmp`/`cmp_arith` are the `local: None` forms and keep the class-rank convention for a mixed internal/external pair.
  Adopters: `beam_bifs`' `term_conv`/`term_ops` comparison BIFs (`<`/`>`/`=/=`/`==`/`sort`) should call `cmp_arith_with_local_node`/`cmp_with_local_node` with the runtime's own `(sysname atom, creation)`; `beam_process`' pid ordering in monitors/links/`'DOWN'` needs the same entries over terms in exact mode, since `ProcessId::to_eterm` produces the internal form whose node is implied by the runtime.
  `beam_ets`' `ordered_set` deliberately keeps `cmp_arith` (a table's ordering must not depend on which node asks), and `cmp_flatmap_keys` stays canonical/context-free for the same reason.
- **`beam_message` may delegate its bounded walk to `size_object_bounded`.** `beam_message/src/copy.rs` re-implements a bounded, cycle-safe sizing walk (`bounded_words`, `is_acyclic`, `preflight_words`); `size::size_object_bounded` is exactly that machinery.
- **Adopt `immediate::try_make_pid_data` instead of re-deriving the pid bound.** Three call sites still write the `ERTS_MAX_INTERNAL_PID_*` check themselves: `beam_codec`'s local-node demotion (`decode/local_node.rs` keeps crate-local `MAX_INTERNAL_PID_NUMBER`/`MAX_INTERNAL_PID_SERIAL`, `external.c:3176-3179`), `beam_bifs::term_conv`'s `list_to_pid` reading (`bif.c:4582-4583`), and `beam_process`'s `ProcessId::to_eterm` (which reaches the infallible, unchecked `make_pid_data`).

## Known Issues
- **Comparison gaps (`src/compare.rs`).** `cmp`/`eq`/`cmp_arith` faithfully reproduce `erts_cmp_compound` for atoms (by name), numbers, tuples, lists, bitstrings, maps (flat *and* hash — size first, then the `(key,value)` sequence in flat-map key order), internal pids/ports, and structurally, field by field, external pids/ports/references and native records.
  Funs are ordered structurally (`cmp_funs`): `Local` by `(uniq, index)`, then `num_free`, then env; `External` by `(module, function, arity)`, after any local kind; `Imported` by `(module, index, old_uniq)`, then `num_free`, then env; a mixed `Local`/`Imported` pair leads with a kind rank (the `Local` descriptor carries no module — a documented deviation from the C).
  The classes without a faithful layout-aware arm use a **deterministic, panic-free `fallback_cmp`** and must be extended for exact ordering: on-heap bin refs (class rank, then object arity, then raw term word); internal references (raw payload words — for the M1 block those words *are* the identity, so equal id ⇒ `=:=` and ids order numerically); atoms whose name is absent from the passed `AtomTable` (raw index).
  Hash-map comparison cannot be byte-matched to ERTS (ERTS's HAMT order depends on its hash), so the flat-map-key-order rule is used for both shapes.
  **Mixed internal/external pid, port and ref pairs**: the context-free `cmp`/`cmp_arith` decide them by the class rank (`external_*` below its internal counterpart) — the documented no-context convention — while `cmp_with_local_node`/`cmp_arith_with_local_node` reproduce the C's field order (`erts_cmp_compound`: pid `serial` → `number` → node, `pid_common` utils.c:1777-1811; port **node only**, the numbers are never compared, :1751-1764/:2219-2233; ref node → ids, `ref_common` :2262-2287), with `CMP_NODES` (:1715-1723) ported as `(sysname atom name, creation)` since no node registry exists here.
  Two documented conventions for the cases the C asserts or cannot reach: a **same-node port** pair (the C raises `ERTS_INTERNAL_ERROR`) compares `internal_port_data` with the external 64-bit number, and the M1 internal ref's single 60-bit id is expressed as the id array `[id as u32, (id >> 32) as u32]` (high word at the high index) so it goes through the *same* `cmp_ref_ids` trim/compare the external arm uses.
  Same-node same-field pid/ref pairs fall through to `Equal` (as `CMP_NODES` no-ops). `tests/compare_class_order.rs` pins both orders, every field-precedence step, the `local: None` ladder, the full 18×18 no-change matrix and the oracle's own verdicts.
- **`Atom` identity is table-relative, not global.** `Atom` is a bare index into one `AtomTable`, so two atoms carry the same `Eterm` only if their table interned names in the same order; the `cmp_impl` fast path (`a.raw() == b.raw()` ⇒ `Equal`) then makes two atoms with the same index but different names compare `Eq`, and an out-of-range index falls back to raw-index comparison (the `_` arm of `cmp_atoms`) — silently breaking **ordering** only, since equality stays faithful.
  Name-faithful only when operands and the passed table share one table; a per-module `AtomTable` (as `beam_loader::Module` builds) makes cross-module atom comparison unreliable.
- **`term_hash` is internal, not compatibility-pinned (`src/hashing/mod.rs`)** — it guarantees only `eq(a,b,atoms) ⇒ term_hash(a,atoms) == term_hash(b,atoms)`.
  Maps hash structurally (sorted pairs), funs over exactly the fields `compare` compares, native records over exactly the fields `cmp_records` compares (the definition address is never mixed, so a record equals its copy hash-wise and `beam_ets`'s CHAMP invariant holds); external pid/port carry no pointer in their words, so their raw-word hash is content-stable, while internal refs and on-heap bin refs use an opaque raw-word fallback (not stable across processes/images).
  `hash_words` mixes an external-ref block's header/arity while `cmp_external_refs` trims the longer id slice's leading **zero** ids, so two external refs of different id-array lengths whose extra ids are zero compare `Equal` yet hash differently — reachable only with a forged/foreign ref.
- **The HAMT is `erts_map_hash`-driven (`src/map.rs` + `src/ihash.rs`).** The trie is placed by the bit-exact `erts_map_hash` port, so a `write_map` hash map's stored pair order equals the oracle's `maps:to_list/1` order (`tests/map_hash.rs`).
  Atom keys are placed correctly only when the `AtomTable` matches the oracle's indices (seeding from `atom.names` gives parity); the C hash is itself non-portable across VM instances ("Limitation #1", it mixes pointers), so the pointer-derived classes use a documented deterministic substitute (`src/ihash.rs` docs).

## Notes for Agents
- **Read `../../erts/emulator/beam/erl_term.h` before changing any constant** — the ground truth; cite it where a value is non-obvious.
- **`TAG_LITERAL_PTR` contract.** `TAG_LITERAL_PTR` (bit `0x4`) sits *above* the 2-bit primary tag and *inside* the 3-bit `TAG_PTR_MASK` (0x7), so `boxed_val`/`list_val` resolution is unchanged by masking it off.
  The C enables it on 64-bit when there is no OS physical-memory reservation (the common case) or under DEBUG; we always define it (Level-1 fidelity).
  `is_literal` is a bare bit test for `0x4`, meaningful **only** on BOXED/LIST words (an IMMED1 word may have that bit set for unrelated reasons).
- `is_list(NIL)` is **false** — NIL is an `IMMED2` immediate, so "a list that is not nil" is exactly a cons cell; `List` terms are single words (they must stay so for the NIF/ETF/msg ABI).
- `make_arityval` encodes `size << 6` with subtag `0x00`, so arity 0 encodes to the raw word `0`; table cells/`[Eterm; N]` register files must not treat `0` as "empty" without care.
- **No fully safe dereferencing accessor layer.** Every accessor that reads a boxed pointer's header word is `pub unsafe fn` (`car`/`cdr`, `is_tuple`/`tuple_size`/`tuple_element`, the `is_*` kind predicates, `float_value`/`float_bits`, `bitstring_size`, `class`/`is_integer`/`is_number`, the `thing.rs` `*_parts` readers, and the sibling-facing `map::{write_map,map_put,map_remove}`, `bitstring::{bitstring_parts,bit_at,write_heap_bits,…}`, `refc::{bin_ref_parts,refc_bytes,…}`, `record::{write_record,record_get,record_is_native,record_definition_*}`).
  Safe `fn`/`const fn` cover only raw-bit inspection (primary tag, immediates, `is_list`/`is_nil`, `is_boxed`/`is_header`, the `*_header` subtag tests, `header_subtag`/`map_header_*`/`arityval`, `boxed_val`/`list_val`/`tuple_val`) plus `cmp`/`eq`/`term_hash`/`size_object`/`AtomTable` access, the safe map readers `map::{map_size,map_get,map_pairs,map_pairs_storage_order}` and `number::{big_to_f64,number_as_f64}`.
  A `#![deny(unsafe_code)]` consumer must deref `*boxed_val(t)` itself to reach into a tuple/list/map/float/binary; a `*_header` subtag test takes a *header word*, not a boxed term.
- Interior layouts NOT externally pinned (list chunking, ETS tree shape) are open and documented at their own nodes; the map layout *is* owned here (`src/map.rs`) because `beam_gc` sizes it — keep it byte-compatible with the copier.
- **Lowering an allocation count here moves sibling exact-count pins** — they are deliberately exact, so a *reduction* fails them just like a regression and the owner must re-derive the constant: `beam_codec/tests/alloc_count.rs` pins the whole decode-path total (`ENCODE_ALLOCS`/`DECODE_FRESH_ALLOCS`/`DECODE_WARM_ALLOCS`) and `beam_codec/CONTEXT.md` quotes it; `beam_bifs/tests/decode_hot_path.rs` and `beam_rt/tests/atom_table_commit_alloc.rs` use `AtomTable::clone` as a Θ(N) sensitivity control, so `Clone` must stay a deep copy.

## See Also
- `../../CONTEXT.md` — workspace contracts (§2 frozen term layout, §3 error strategy).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.1–A.3, §B; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A, §B.
- `../../erts/emulator/beam/CONTEXT.md` — legacy core; its "Term Encoding (Eterm) Reference" section.
- `../beam_abi/` — NIF/driver C ABI; transmutes `Eterm` ↔ `ERL_NIF_TERM`.
- `../beam_alloc/`, `../beam_gc/` — production heap/allocator + collector built on `heap.rs`.
