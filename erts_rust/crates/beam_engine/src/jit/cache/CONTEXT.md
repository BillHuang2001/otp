# `beam_engine` baseline-JIT code cache (`src/jit/cache/`)

## Intent
The cross-run cache of compiled **baseline-tier** artifacts (`CompiledModule`) reused by every
`Machine` on a thread. It is the lookup behind every cached slice entry and resume
(`run_baseline_cached*`) and behind the native driver's cross-module callee resolution
(`cached_module_blocks`). Its one load-bearing job: identify a module's artifact in **O(1)**, so a
slice entry/resume never re-hashes the module's decoded IR.

## API Surface
- `CodeCache` — `new`/`len`/`is_empty`/`compiles`/`contains`/`clear`/`get_or_compile`/`compiled_handle`/`run_export`/`run_export_outcome`/`run_label_outcome`/`code_map`. Owns the artifacts and their executable mappings.
- Free functions over the per-thread `CODE_CACHE`: `run_baseline_cached{,_outcome,_label_outcome,_resume_outcome}`, `cached_module_blocks`, `baseline_code_map`, `baseline_cache_len`, `baseline_compile_count`, `clear_baseline_cache`, and the host-facing `remember_module(&Arc<Module>)` (infallible; a documented no-op, never a panic, while the cache is already borrowed).
- The four cached entries are the whole `Tier::BaselineJit` runtime surface reachable from `Machine::run_*_with_tier` (see `../CONTEXT.md`).

## Identity (the cache key)
`ModuleKey` (private) is an enum:
- `Digest([u8; 16])` — a real `.beam`'s **load-time MD5** (`beam_loader::Module::checksum()`), read as a plain `Copy`. O(1), no hash, no allocation. This is the point of the design.
- `Content { name: String, fingerprint: u64 }` — the fallback for a synthetic/hand-built module (`Module::default()` carries an all-zero checksum, which no real `.beam` has): its name plus the `module_fingerprint` content hash.
`ModuleKey::of` / `with_fingerprint` and the helpers `digest_of`/`module_name` derive the key; the two variants hash their discriminant, so they can never cross-key. `module_fingerprint` stays `pub` (the fallback identity).

## Soundness precondition (load-bearing)
`Digest` is a sound artifact identity **iff a loaded module is immutable after load**.
The MD5 digests the code-relevant chunks (`Atom`/`AtU8`/`Code`/`StrT`/`ImpT`/`ExpT`/`FunT'`/`LitT`/`Meta`/`Recs`/`DbgB`, per `../../../beam_loader/src/checksum.rs`) and excludes `Line`/`LocT`/`Attr`/`CInf`/`Type`/`Docs`/`Dbgi` — none of which the JIT artifact reads (they are read from the live module by the interpreter families / `machine/catch.rs`).
Verified: no production path mutates a `Module`'s `functions`/`funs`/`records`/`literals`/`exports`/`imports`/`checksum`; every such write is inside a `#[cfg(test)]`/`tests/` module, and the code server wraps loaded modules in `Arc` and only reads.
**A post-load mutation would invalidate the key** — the accepted trade for O(1).

## Known Issues
- The **one-time cold staging** of a module still costs exactly one `module_fingerprint`: `CompiledModule::lazy` / the eager constructor (in `../baseline/`) compute the artifact's stored `fingerprint` at creation. The digest key removes all **per-slice** cost, not that one-time construction cost.
- The address-keyed resolution memo (`memo`/`MemoEntry`) is retained and is now a redundant micro-optimisation on top of the O(1) digest; it is load-bearing only for a synthetic (zero-checksum) module.
- A synthetic zero-checksum module still pays one content hash per **un-memoized** lookup — the mandated fallback.
- Integration test `../tests/jit_cache.rs::same_name_different_content_does_not_collide` clones a loaded module (inheriting its MD5), mutates it, and asserts a cache miss — false under the digest key by construction (post-load mutation is impossible in production). Needs the `tests/` owner to re-point it (e.g. give the clone a distinct checksum, or use synthetic modules as `tests.rs` does); `tests/` is a sibling of `src/` (read-only here).

## Routing Table
- `mod.rs` → `CodeCache`, the per-thread `CODE_CACHE`, the `run_baseline_cached*` / `cached_module_blocks` / `remember_module` free functions, and the `module_fingerprint` re-export.
- `key.rs` → the key machinery: `CacheKey`/`ModuleKey` (+ `of`/`with_fingerprint`), the `module_fingerprint`/`HashWriter` fallback content hash, `digest_of`/`module_name`/`module_address`. Split out of `mod.rs` when the cache plus its key machinery exceeded the ~1000-line budget.
- `tests.rs` → the counter tests (deterministic `fingerprint_calls()`, no wall-clock): real-module zero-rehash, digest-keyed artifact sharing, the synthetic fallback, and priming/entry-path coverage.
