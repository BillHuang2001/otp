# `beam_engine` baseline-JIT code cache (`src/jit/cache/`)

## Intent
The cross-run cache of compiled **baseline-tier** artifacts (`CompiledModule`) reused by every
`Machine` on a thread. It is the lookup behind every cached slice entry and resume
(`run_baseline_cached*`) and behind the native driver's cross-module callee resolution
(`cached_module_blocks`). Its one load-bearing job: identify a module's artifact **soundly and in O(1)** — by its identity **and** its atom namespace — so a
slice entry/resume never re-hashes the module's decoded IR.

## API Surface
- `CodeCache` — `new`/`len`/`is_empty`/`compiles`/`contains`/`clear`/`get_or_compile`/`compiled_handle`/`run_export`/`run_export_outcome`/`run_label_outcome`/`code_map`. Owns the artifacts and their executable mappings.
- Free functions over the per-thread `CODE_CACHE`: `run_baseline_cached{,_outcome,_label_outcome,_resume_outcome}`, `cached_module_blocks`, `baseline_code_map`, `baseline_cache_len`, `baseline_compile_count`, `clear_baseline_cache`, and the host-facing `remember_module(&Arc<Module>)` (infallible; a documented no-op, never a panic, while the cache is already borrowed).
- The four cached entries are the whole `Tier::BaselineJit` runtime surface reachable from `Machine::run_*_with_tier` (see `../CONTEXT.md`).

## Identity (the cache key)
The key is the two-part `CacheKey { module: ModuleKey, atoms: AtomSnapshot }` — **which module**, and **which atom namespace** its code was lowered against.
`ModuleKey` (private) is the module half, an enum:
- `Digest([u8; 16])` — a real `.beam`'s **load-time MD5** (`beam_loader::Module::checksum()`), read as a plain `Copy`. O(1), no hash, no allocation. This is the point of the design.
- `Content { name: String, fingerprint: u64 }` — the fallback for a synthetic/hand-built module (`Module::default()` carries an all-zero checksum, which no real `.beam` has): its name plus the `module_fingerprint` content hash.
`atoms` is `Module::atom_table.snapshot()` — an O(1), allocation-free witness of the table's identity, its append generation and its length.
`CacheKey::of` / `CacheKey::with_fingerprint` are the **single** derivation every entry and every `MemoEntry` goes through, so the two maps cannot disagree about a module's identity; `ModuleKey::of` / `with_fingerprint` and the helpers `digest_of`/`module_name` derive the module half, and the two variants hash their discriminant, so they can never cross-key. `module_fingerprint` stays `pub` (the fallback identity, re-exported by `mod.rs`).
The namespace half is load-bearing: the baseline tier bakes atom **indices** into its emitted code (`Operand::Atom(a)` → the `make_atom(a)` immediate), so the same `.beam` loaded against a second namespace (a second runtime in the same thread, which is how `beam_rt`'s code server loads every `.beam`) must **miss** instead of reusing the first runtime's blocks.

## The trade
Two separately loaded copies of one `.beam` — or a `Module::clone`, since `AtomTable::clone` mints a **fresh** identity — are two entries, each a miss then a hit; a stale generation of the same table is likewise a miss, and reusing the **same** `Module` object still hits.
The cost is at worst an extra compile, never a wrong answer: equal snapshots imply the same index → name mapping over every index the artifact can embed, and reading one is O(1) (three scalars), so a real `.beam`'s hit path stays hash-free and allocation-free.
The address-keyed resolution memo is honoured only while its `Weak<Module>` upgrades **and** the module's current snapshot still equals the memoized key's (a stale generation recompiles).

## Soundness precondition (load-bearing)
`Digest` is a sound *module* identity **iff a loaded module is immutable after load**.
The MD5 digests the code-relevant chunks (`Atom`/`AtU8`/`Code`/`StrT`/`ImpT`/`ExpT`/`FunT'`/`LitT`/`Meta`/`Recs`/`DbgB`, per `../../../beam_loader/src/checksum.rs`) and excludes `Line`/`LocT`/`Attr`/`CInf`/`Type`/`Docs`/`Dbgi` — none of which the JIT artifact reads (they are read from the live module by the interpreter families / `machine/catch.rs`).
The one digested field whose meaning is namespace-dependent — the `AtU8` atom table — is exactly what the `atoms` half of the `CacheKey` witnesses, so the digest is artifact-equivalent for every namespace-independent operand.
Verified: no production path mutates a `Module`'s `functions`/`funs`/`records`/`literals`/`exports`/`imports`/`checksum`; every such write is inside a `#[cfg(test)]`/`tests/` module, and the code server wraps loaded modules in `Arc` and only reads.
**A post-load mutation would invalidate the key** — the accepted trade for O(1).

## Known Issues
- The **one-time cold staging** of a module still costs exactly one `module_fingerprint`: `CompiledModule::lazy` / the eager constructor (in `../baseline/`) compute the artifact's stored `fingerprint` at creation. The digest key removes all **per-slice** cost, not that one-time construction cost.
- The address-keyed resolution memo (`memo`/`MemoEntry`) is a redundant micro-optimisation on top of the O(1) key; it is load-bearing only for a synthetic (zero-checksum) module, and its entry is honoured only while its `Weak<Module>` upgrades **and** the module's current snapshot still equals the memoized key's.
- A synthetic zero-checksum module still pays one content hash per **un-memoized** lookup — the mandated fallback.

## Routing Table
- `mod.rs` → `CodeCache`, the per-thread `CODE_CACHE`, the `run_baseline_cached*` / `cached_module_blocks` / `remember_module` free functions, and the `module_fingerprint` re-export.
- `key.rs` → the key machinery: `CacheKey`/`ModuleKey` (+ `of`/`with_fingerprint`), the `module_fingerprint`/`HashWriter` fallback content hash, `digest_of`/`module_name`/`module_address`. Split out of `mod.rs` when the cache plus its key machinery exceeded the ~1000-line budget.
- `tests.rs` → the counter tests (deterministic `fingerprint_calls()`, no wall-clock): real-module zero-rehash, digest-keyed artifact sharing, the synthetic fallback, priming/entry-path coverage, and the namespace-truth pins `two_loads_of_one_beam_are_two_namespaces_and_two_entries` and `two_loads_of_the_same_beam_share_the_digest_half_but_not_the_namespace_half`, plus `a_second_atom_namespace_is_never_handed_the_first_namespaces_artifact` — the pin that fails without the namespace half.
