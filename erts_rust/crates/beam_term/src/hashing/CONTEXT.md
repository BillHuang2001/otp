# `hashing/` — `phash2`/`phash` (pinned) + the internal structural hash

## Intent
Bit-exact ports of `../../../../erts/emulator/beam/erl_term_hashing.c` — the values `erlang:phash2/1,2` and
`erlang:phash/2` return — plus the crate-internal structural hash the runtime uses for its own tables.
Ground truth: `erl_term_hashing.c`; `erl_record.c:1041-1046` for the record definition hash.

## API Surface
| File | Owns |
|---|---|
| `mod.rs` | module docs + `term_hash` (internal) + the structural-hash and oracle-vector suites |
| `common.rs` | the `pub(super)` ERTS-hash primitives both ports share |
| `make_hash2.rs` | `make_hash2`/`make_hash2_with_identity` + `compute_record_definition_hash` |
| `make_hash.rs` | `make_hash`/`make_hash_with_identity` + `hash_binary_bytes` |
| `identity.rs` | the `LocalFunIdentity` provider trait both ports consult for a `FunKind::Local` fun |

### The pinned contract (`make_hash2`, `make_hash`)
`make_hash2(t, atoms)` and `make_hash(t, atoms)` (`-> TermResult<u32>`) are **bit-exact u32-wrapping** ports,
oracle-cross-checked; unlike `term_hash` **these ARE part of the Level-1 drop-in contract**.
They cover every class this crate can represent, after the C's `hash == 0` fast paths.
Consumers: `beam_bifs` (`phash2`/`phash`, `erts_internal:phash2`), `beam_ets`.
`hash_binary_bytes` is the byte-payload half, shared so an atom-index table and a binary never drift.

### `compute_record_definition_hash(def, atoms)`
The synthetic definition hash the C caches — named `compute_*` to avoid colliding with the header-*reader*
`record::record_definition_hash`.
Both ports mix a record's **real computed** definition hash, never the `definition_hash` word the block
stores, and walk the values in **sorted-key order** (the order `record.rs` stores them).

### The fun-identity provider (`Local` funs are hashable)
`pub trait LocalFunIdentity { fn local_fun_module(&mut self, term: Eterm) -> TermResult<Option<Atom>> }`
supplies the one field a `Local` fun's descriptor cannot carry — the **defining module** whose atom-table
`hvalue` the C mixes (`erl_term_hashing.c:249`/`:1232`).
Entry points: `make_hash2_with_identity(t, atoms, Option<&mut dyn LocalFunIdentity>)` and
`make_hash_with_identity(...)`; `make_hash2(t, atoms)`/`make_hash(t, atoms)` keep their signatures and
delegate with `None`.
A provider is asked **once per `Local` fun the walk reaches** (a closure nested in a tuple/list/map
included), answers `Ok(None)` for "I cannot attribute this term", and may report its own failure
(`TermResult`), so a failing provider surfaces its own error instead of a substituted hash.

### `term_hash` — internal, NOT compatibility-pinned
`term_hash(a, atoms) -> u64` is deterministic and guarantees only `eq(a,b,atoms) ⇒ equal hashes`; its exact
values are not part of any contract.
Maps hash structurally (sorted pairs), funs over exactly the fields `compare` compares, native records over
exactly the fields `cmp_records` compares (the definition address is never mixed, so a record equals its
copy hash-wise and `beam_ets`'s CHAMP invariant holds).
External pid/port carry no pointer in their words, so their raw-word hash is content-stable; internal
references and on-heap bin refs use an opaque raw-word fallback (not stable across processes/images).

## Constraints
- **Identity gaps must never become a wrong value.** `FunKind::Local` ⇒ `TermError::FunIdentityUnavailable`
  when no provider (or a `None` answer) attributes it — the 2-word descriptor inlines only `uniq`/`index`
  (the documented M1 substitution for the C's out-of-line `ErlFunEntry`), so with a provider the fun hashes
  exactly like the equivalent `NEW_FUN_EXT` image.
  An **internal reference** ⇒ `TermError::RefIdentityUnavailable` (still open: the C mixes
  `internal_ref_numbers(term)[0]`, a node-container projection this layout does not expose).
- No dependency edge beyond `beam_common`/`beam_term`'s own modules; the ports take the `AtomTable` as a
  parameter (an atom's `hvalue` is table state, never global).
- `unsafe` (payload-word dereference) stays confined to the marked walk sites.

## Test Strategy
`../../tests/record_ref_hash.rs` pins offline `(make_hash2, make_hash, definition_hash)` triples for a native
record and the reference shapes, plus a real `-record` fixture cross-check.
`mod.rs`'s in-module suites pin the structural hash's `eq ⇒ equal hash` invariant and carry the
oracle-derived vectors; `../../tests/map_hash.rs` and `../../tests/oracle.rs` cross-check the map/bignum
shapes against a live `erl` (skip loudly when absent).

## Routing Table
- `mod.rs` → `term_hash` (internal structural hash) + docs + suites
- `common.rs` → shared ERTS-hash primitives (`pub(super)`)
- `make_hash2.rs` → `make_hash2`/`make_hash2_with_identity`/`compute_record_definition_hash`
- `make_hash.rs` → `make_hash`/`make_hash_with_identity`/`hash_binary_bytes`
- `identity.rs` → `LocalFunIdentity` provider trait
- `../ihash.rs` → `erts_map_hash` (the *map* hash, a separate algorithm — not this node)
