# `beam_bifs` — BIF Registry (`bif.tab` Surface) & Implementations

## Intent
Owns the **Built-In Function layer** (Stage 5, Part II §H): the `(module, function, arity)` → implementation
registry plus the curated BIF surface. Contract **Part I §A.7** — names/modules/arities reproduce
`../../erts/emulator/beam/bif.tab`; none is invented. `beam_rt` adapts `BifRegistry` into the engine's
`ExecutionContext::call_bif(module, function, arity, args) -> Result<Eterm>` seam (see `../beam_engine/CONTEXT.md`).
Failure is **return-based** (`../CONTEXT.md` §3): `BadArg`/`BadArith`/`BadArity`/`SystemLimit`, plus
`BeamError::Raise` for `error`/`exit`/`throw`/`nif_error`/`undef`/`unsupported_bif`, which the engine turns into a
BEAM raise (exception tag in `x(0)`) — no `setjmp`/`longjmp`. **A typed error always beats a wrong result**: a
shape that cannot be derived faithfully returns the typed gap, never a fabricated value.
Ground truth (read-only): `../../erts/emulator/beam/` (files in the Routing Table),
`../../erts/emulator/sys/common/erl_float_short.cpp`, and the stdlib sources
`erlang.erl`/`erts_internal.erl`/`ets.erl`/`lists.erl`/`maps.erl`/`io_lib*.erl`/`unicode.erl`/`string.erl`.

## Roster census (authoritative; code-derived, pinned by `tests/roster_audit.rs`)
`bif.tab` declares **556** entries (`bif` 476, `ubif` 73, `hbif` 7); **0 are missing**. `default_registry()` =
`BifRegistry::with_core_bifs()` = **648 keys** = **556 declared + 92 stdlib fast paths** (exports `bif.tab` does
not declare: `lists` +40 over its 5 declared keys — 18 of them fun-taking — `maps` +12 over its 12, 8 `ets:*`,
`io`/`io_lib`, `unicode`, `binary:referenced_byte_size/1`, the OTP-30 `pid_to_binary`/`ref_to_binary`/
`port_to_binary`, `erlang:memory/0,1`, `erlang:convert_time_unit/3`, `erlang:fun_info/1`, the six `zlib` fast paths —
`compress/1`/`uncompress/1`/`zip/1`/`unzip/1`/`gzip/1`/`gunzip/1`). Re-derive
any split from the registry, never from this prose.
**Declared ≠ registered ≠ adopted.** Of the 556, **346 are unconditional in-crate** and **210 are seam-backed**
(they route through one of the fifteen optional `BifContext` hooks and answer the typed `unsupported_bif` until a
host installs the hook). The dedup **seam union is 240** = the 13 `hook_sets.rs` tables (244 raw keys − the 4
`LOCAL_FUN_IDENTITY ∩ LOCAL_NODE` keys); **30** union members are *undeclared* in `bif.tab`
(`MEMORY_HOOK_BIFS` 2 ∪ `PORT_HOOK_BIFS` 4 ∪ `FUN_CALLER_BIFS` 24). **209** keys are runtime-coupled and live
under `src/runtime/` (per-key roster: `./src/runtime/CONTEXT.md`). There are **no dead registrations**.
`tests/roster_audit.rs` is the pin: it builds the union by iterating the 13 public tables (never hard-coding a key
list) and asserts the partition (556 = 210 + 346), the union size, the per-table/per-module declared counts (25
modules), and that every union member is registered with its `needs_*` predicate true.
Declared `bif.tab` keys per module (sums to 556): `erlang` 268 · `erts_internal` 71 · `ets` 51 · `math` 24 ·
`binary` 22 · `erts_debug` 20 · `erl_debugger` 13 · `maps` 12 · `os` 10 · `re` 8 · `records` 8 ·
`atomics`/`code`/`erl_ddll`/`persistent_term` 7 each · `lists` 5 · `prim_file` 4 · `unicode` 3 ·
`erts_literal_area_collector`/`erts_trace_cleaner` 2 each · `error_logger`/`file`/`io`/`net_kernel`/`string` 1 each.

## API Surface (FROZEN — `beam_rt` and tests design against this; additive-only changes)
Root + `beam_bifs::prelude::*`: `BifRegistry`/`BifFn`/`default_registry`; `BifContext` + its fifteen seams;
`RecordDefinitionView`/`RecordFieldView`; `StdoutSink`/`NullSink`/`BytesSink`; `NifLoadError`;
`raise_error`/`raise_exit`/`raise_throw`; `exit_reason_from_term`/`reason_term_from_exit_reason`;
`TableRegistry`; the `hook_sets.rs` tables + `needs_*` predicates. `beam_bifs::bif_tab` is **not** in the prelude
(validation tooling, not a runtime seam).
- **`registry.rs`** — `BifFn = fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>`; `BifRegistry` (nested
  `module → function → [(arity, BifFn)]` maps, so `lookup`/`contains` are allocation-free; `new`/`is_empty`/`len`/
  `names`/`entries`); `with_core_bifs()` (the 648-key roster) = `default_registry()`; `register(...) -> Option<BifFn>`
  (replaces on repeat); `call(...)` (unknown key ⇒ `error::undefined_bif`; wrong arg count ⇒ `BadArity`). Operator
  BIFs keep their *quoted* `bif.tab` names (`"+"`, `"=<"`, `"=:="`, `"!"`, `"++"`, `"not"`, …). `with_core_bifs`
  calls each module's `pub(crate) fn register` in a fixed order (`ets` wiring
  `ets::{matches,specs,extra,adopt,nav,reverse}`; `runtime` last, its real `atomics`/`counters`/`erl_ddll` keys
  after the seam families).
- **`context.rs`** — `BifContext<'a>`, the single per-call context: `heap`/`atoms`/`self_pid`/`current_module`,
  one optional slot per seam (a `with_<seam>()` builder + matching accessor), the constructors
  `int`/`atom`/`intern_atom`/`boolean`/`ok`/`make_float`/`tuple`/`cons`/`list`, `pub(crate) atom_and_heap` (the one
  route that borrows the table *and* the destination heap at once), the `ProcessInfoItem` enum and the
  `OutputSink` impls.
- **The fifteen optional seams:** `OutputSink` (`write_str`); `SendHook` (`send`); `AtomInterner` (`intern`);
  `FunCaller` (`call_fun`); `PortHook` (`open_port`/`close_port`/`port_info`); `DictHook`
  (`put`/`get`/`erase`/`get_keys`/`get_keys_for_value` + defaulted `get_all`/`erase_all`); `PersistentTerm`
  (`get`/`put`/`erase`/`get_all`/`info`); `NifLoader` (`load_nif`); `RecordSource` (`record_definition`);
  `LocalNode` (`local_node`); `LocalFunIdentity` (`local_fun_identity`); `ModuleInfo` (`module_info`);
  `ProcessHook` (`whereis`/`register`/`unregister`/`registered`/`is_process_alive`/`processes`/`ports`/
  `process_info_keys`/`process_info_item`/`process_flag`/`group_leader`); `MemoryHook` (`memory`); `RuntimeHooks`
  (`call`).
- **Every hook is defaulted:** with none installed the BIF answers the typed `unsupported_bif` gap (or a
  documented `Ok(None)`/fallback for a value-returning seam) — **never** a fabricated empty list, `undefined`,
  `false`, zero or made-up count. Unset defaults: stdout for `io:format`; `unsupported_bif` for
  `send/2`/`"!"/2`/`send/3`, the port/dict/`PersistentTerm`/`ProcessHook`/`ModuleInfo` BIFs, `load_nif/2`, and
  the fun-taking `lists`/`maps` BIFs **for a valid fun** (an invalid fun is rejected by the clause guard first,
  see the `FunCaller` section); `TermNotFound` for a new-atom BIF; `Ok(None)` for
  `record_definition`/`local_fun_identity`/`module_info`; `(nonode@nohost, 0)` for `local_node`, else `None` when
  that atom is not interned here.
- **`hook_sets.rs`** is the one authoritative home of the *seam-consumer* key sets: a `&'static` table per seam +
  an allocation-free `needs_*` predicate, re-exported at the root and in the prelude, pinned and `contains`-checked
  against the live registry. Sizes: `MEMORY_HOOK_BIFS` 2, `LOCAL_FUN_IDENTITY_BIFS` 4, `DICT_HOOK_BIFS` 7,
  `PROCESS_HOOK_BIFS` 11, `PORT_HOOK_BIFS` 4, `SEND_HOOK_BIFS` 3, `NIF_LOADER_BIFS` 1, `RECORD_SOURCE_BIFS` 1,
  `MODULE_INFO_BIFS` 2, `PERSISTENT_TERM_BIFS` 7, `FUN_CALLER_BIFS` 24, `LOCAL_NODE_BIFS` 6, `RUNTIME_HOOKS_BIFS`
  **172** (asserted *equal* to `runtime::register`'s seam roster) — the 13 tables total **244** keys and the
  dedup union is **240**. The module doc's per-row counts *and* its aggregate are pinned against the tables by
  `the_module_doc_table_counts_match_the_tables` (a table added without a doc row, or any changed total, fails the
  build). `AtomInterner` deliberately has **no** table — its consumer set is unbounded. A host gates its seams
  through these, never a private copy.
- **`error.rs`** — `pub(crate)` `badarg`/`badarith`/`badarity`/`system_limit`/`undefined_bif`/`unsupported_bif`/
  `function_clause`; public `raise_error`/`raise_exit`/`raise_throw` over
  `beam_common::{Raise, ExitReason, ExceptionClass}`; `raise_term`/`raise_error_term` set
  `reason = ExitReason::Structured` with the term in the opaque `Raise::with_live_reason`/`LiveReason` slot
  (`as_atom()` stays `None`; `PartialEq`/`Debug` ignore the payload); `structured_error` builds
  `{Tag, Elements…}`, degrading to plain `badarg()` when `tag` cannot be interned; `function_clause()` raises the
  bare atom.
- **`reason.rs`** — `exit_reason_from_term(term, &AtomTable) -> ExitReason` (`normal`/`shutdown`/`noproc`/
  `noconnection`/`killed`/`aborted` ⇒ their variants, any other atom ⇒ `Atom(name)`, non-atom/unknown ⇒
  `Structured`); `reason_term_from_exit_reason` rejects `Structured` with `BadArg`; the `error/1,2,3`, `exit/1`,
  `throw/1`, `nif_error/1,2` BIFs run their argument through it — an **atom** reason keeps its exact `ExitReason`
  with **no** live slot, a **non-atom** one is `Structured` carrying the term.

## Per-module contracts
One entry per module: the **contract** (what is registered, what the oracle semantics are) and the
**oracle-verified traps**. Algorithm detail lives in each module's own rustdoc; for the sub-nodes (`src/ets/`,
`src/re/`, `src/packet/`, `src/runtime/`, `src/print/`, `src/unicode/`) it lives in that node's `CONTEXT.md`.
### `re` (`src/re/`) — the 8 `re:*` BIFs, served for real
All 8 declared keys (`version/0`, `compile/1,2`, `run/2,3`, `internal_run/4`, `inspect/2`, `import/1`) are real
`BifFn`s (`src/runtime/other.rs` calls `crate::re::register`), not seam keys. Self-contained safe-Rust engine with
**no third-party regex crate** (licensing: Apache-2.0/MIT only) and no `unsafe`; the compiled pattern's
element 5 is **our own** versioned image, never PCRE2's `#Ref<…>`. `{newline,crlf}` is positional (only the pair
`\r\n` is a newline); `{newline,none}` is `badarg`. Contract + gaps: `./src/re/CONTEXT.md`.
### `records` (native records, `records.rs`) — 8 BIFs
OTP-29 native records; the frozen interior is `beam_term::record` (the LCA); ground truth `erl_record.c`. `create/4`'s
cached definition word is the **real** `beam_term::hashing::compute_record_definition_hash` value (written through
`beam_codec::term::make_record`), and `beam_term::hashing` recomputes it rather than reading word 2, so `phash2` is
unaffected by the stored word. `update/4` takes a map (flat **or** HAMT — the C has an arm for each),
accepts `'_'` as the module/name wildcard, returns the record unchanged for an empty map, and requires every key to
name an existing field (`=:=`) in declaration order. `get_definition/2` needs the `RecordSource` hook ⇒
`{#{is_exported => B}, Fields}` in declaration order (no hook ⇒ `badarg`). `is_record/2` is true for a tuple **or** a
native record. Structured reasons carry their term (`{badrecord,T}`, `{badfield,{{M,N},K}}`, `{badmap,M}`).
### Maps (`maps.rs`) — both representations, no pair-count ceiling
A flat map (≤ `MAP_SMALL_MAP_LIMIT` = 32 distinct keys) **and** a HAMT above it, both `beam_term::map`; every reader
and producer (the `maps:*` BIFs, `map_get/2`/`map_size/1`/`is_map_key/2`, the printers, `records:update/4`) handles
either. The writer matches ERTS (≤32 ⇒ flat in `erts_cmp_flatmap_keys` order, else a HAMT keyed by `erts_map_hash`)
and collapses duplicate keys **last value wins**. Key identity is exact (`=:=`), pair order is the *stored* order, and
a missing key / non-map raises the structured `{badkey,K}`/`{badmap,M}`. `maps:with/2`/`without/2` classify the two
arguments **together** (`error_type(M)`): the MAP alone decides — non-map ⇒ `{badmap,M}`; non-list Keys + a map ⇒
`badarg`; improper Keys + a map ⇒ bare `function_clause`. Deviation: the HAMT `iterator/1`/`next/1` path chunking
differs from the C's reducer-count-driven chunking (the emitted `{K,V,Next}` sequence is identical).
### Exact integer arithmetic (`arith.rs`), the bit operators, `lists:sum/1`
`+ - * div rem abs` and unary `-`/`+` are **arbitrary-precision** (sign+magnitude limbs; an out-of-60-bit result
becomes a heap bignum via the **safe** `beam_codec::term::make_big`); `div`/`rem` are Knuth D; `add_terms` is shared
with `lists:sum/1`, and `int_from_mag`/`int_from_i64`/`int_from_f64` are the single promotion path for every
integer-producing BIF. The bit operators are **integer-only** (`1 band 1.0` ⇒ `badarith`) and two's-complement
arbitrary-precision, and **a negative count flips `bsl`⇄`bsr`** (`8 bsl -2` ⇒ `2`); `bnot(A)` = `-A-1`. Cap
`BIG_ARITY_MAX = (1<<16)-1` limbs = 4_194_240 bits ⇒ `system_limit`, an oversized left shift rejected before
allocation (`1 bsl 4194239` succeeds).
### `math` (24 keys) / the strict booleans (`bool.rs`)
`math` are real BIFs over `f64`: a unary call coerces exactly as C `math_call_1` and maps a post-call **domain error
or overflow to `badarith`** (never a wrong float). `atanh/1` is **bit-exact**; `erf`/`erfc` are approximate (Known
Issues). `bool`: `not/1`, `and/2`, `or/2`, `xor/2` are **strict** (`true`/`false` only, any other term
`error:badarg`); the short-circuiting **`andalso`/`orelse` are NOT BIFs** (the compiler lowers them to jumps).
### `erlang:phash/2` + `phash2/1,2` (`compare.rs` — thin adapters)
The hashes are the LCA's compatibility-pinned `beam_term::hashing::{make_hash, make_hash2}` (transcriptions of
`erl_term_hashing.c`), never re-implemented here; these BIFs only build the `Range` and reduce — `phash/2` =
`1 + hash % Range`, `phash2/1` = `hash & 0x07FF_FFFF`, `phash2/2` = `hash % Range`; `Range = 0` ⇒ `hash + 1` / the
whole hash; a non-integer/negative/`>0xFFFF_FFFF` Range is `badarg`. Goldens and sweeps: `tests/phash.rs`.
**Typed-gap contract:** `beam_term::hashing` is authoritative (no consumer-side pre-walk) and every `TermError` maps
to the typed `unsupported_bif`, never a `?`-collapsed `badarg`.
### List operators (`lists.rs`)
`'++'/2` requires a **proper-list LEFT operand** and uses the RIGHT operand **verbatim with no check**
(`[1,2] ++ x` ⇒ `[1,2|x]`, `[] ++ x` ⇒ `x` — an intentional bug-compatibility); `'--'/2` removes from the left the
**first** remaining occurrence of each right element, matching by **exact `=:=`** (`[1.0] -- [1]` ⇒ `[1.0]`). The 45
registered keys = 5 declared + 40 `lists.erl` fast paths. Comparison order is **per function, pinned to the oracle**:
exact (`eq`) for `member/2`/`delete/2`/`keymember/3`/`keysearch/3`/`'--'/2`, arithmetic (`cmp_arith`) for
`keyfind/3`/`sort/1`/`usort/1`/`keysort/2`/`max/1`/`min/1`. `sort/2`, `usort/2`, `mapfoldl/3` and `mapfoldr/3` are
faithful `lists.erl` ports (`fsplit_1`/`fsplit_2`/`fmerge`/`rfmerge`; `usort_1`/`ufsplit_*`/`rufmerge*`) over
`BifContext::call_fun`, so a **degenerate comparator reproduces the oracle's reversal**
(`lists:sort(fun(_,_) -> false end, [1,2,3])` ⇒ `[3,2,1]`, strict-key ties descending) and a non-2-tuple fun result
raises the structured `{badmatch, Value}`.
### iolist / bitstring BIFs (`binary.rs`)
`iolist_to_binary/1`/`iolist_size/1` are the **Unit-8** walk; `list_to_bitstring/1` the **Unit-1** walk;
`split_binary/2` is the position-based split (`{Before,After}`), distinct from the pattern-based
`binary:split/2,3`. `binary:compile_pattern/1` mirrors `maybe_binary_match_compile`: a **non-empty byte-aligned
binary, or a non-empty list of them**, in a module-local process-global registry, the handle an on-heap `Ref` thing.
Shape rule: **1 "word" of pattern ⇒ `{bm,Ref}`, more ⇒ `{ac,Ref}`**; there is **no** `am` tag. Any other argument
(incl. an integer, an empty list/binary, an improper list, a non-binary element) is `badarg`.
### `erlang:convert_time_unit/3` (`time.rs`) — a stdlib fast path, not a `bif.tab` entry
The oracle implements it as **Erlang code** (`../../erts/preloaded/src/erlang.erl:5338-5384`), so it is registered
here exactly like the sibling `erlang:memory/0,1`; it is what closes `timer:tc/1,2,3,4` (every clause of
`lib/stdlib/src/timer.erl` ends in `convert_time_unit(T2-T1, native, TimeUnit)`). The body is `TU * Time div FU`
after an identity arm — `FU =:= TU` **and** an integer `Time` returns `Time` unchanged, bignum included. `native` and
`perf_counter` both resolve to `1_000_000_000` (`erts_internal:time_unit()`/`perf_counter_unit()`, the fixed native
unit), every other atom is its units-per-second ratio (`UnitRatio::Small`), and a positive **integer** unit is
`UnitRatio::Big`. A negative `Time` uses the C floor `(TU * Time - (FU - 1)) div FU`
(`convert_time_unit(-1500000000,nanosecond,second)` ⇒ `-2`). Bignum ratios go through
`big_ratio_conversion`/`big_ratio_chain`, which build the ratio as a term and re-enter the **registered**
`erlang:'*'/2`/`'-'/2`/`'div'/2` — the conversion is never re-implemented. `erlang.erl` wraps the whole body in
`try … catch _:_ -> error_with_info(badarg, …)`, so **every** failure (an unknown unit atom, a zero/negative unit, a
non-atom non-integer unit, a non-integer `Time`, a product past the bignum cap — `trap_arith_class`) is `badarg`.

### `zlib` (`src/zlib.rs`, `src/deflate.rs`) — the six self-contained fast paths
`zlib` is **preloaded**: `../../erts/preloaded/src/zlib.erl` is real Erlang, and its stream interface
(`open/0`→`deflateInit/2`→`deflate/3`→…→`close/1`) is `erlang:nif_error(undef)` stubs for the statically linked
zlib NIF (`../../erts/emulator/beam/erl_zlib.c`) this runtime does not carry, so six self-contained functions are
registered instead — none is a `bif.tab` entry (the table declares the NIF stubs). All six take **iodata**
(`binary::iodata_bytes`: a bare integer, an atom, an element outside `0..=255` and a non-byte-aligned bitstring are
all `badarg`).
- `compress/1` emits one RFC 1950 stream (2-byte header + deflate + Adler-32) at `DEFAULT_COMPRESSION_LEVEL` = 6 over
  `beam_codec::compress` (the existing `flate2`/`rust_backend` codec — **no new dependency edge**); `uncompress/1`
  requires at least `MIN_UNCOMPRESS_LENGTH` = 8 bytes, and **every** other failure is `data_error`, never `badarg`
  (the length arm is raised *inside* `zlib.erl`'s `try` body, so that body's own `catch _:_ -> badarg` does not cover
  it). It inflates through `beam_codec::compress::decompress_bounded(data, expansion_limit(len))` with the DEFLATE
  format's own `MAX_DEFLATE_EXPANSION` = 1032:1 bound — **no artificial size ceiling**, so any payload the oracle
  inflates inflates here too.
- `zip/1`/`unzip/1` are the **raw DEFLATE (RFC 1951)** pair — *not* gzip: `zlib.erl` opens them with `-MAX_WBITS`.
  `zip/1` strips the 2-byte header and 4-byte Adler-32 off a `beam_codec::compress` stream (the framing and the
  FDICT bit are asserted before the slice is taken); `unzip/1` is the self-contained safe-Rust RFC 1951 decoder in
  `src/deflate.rs`, because `beam_codec`'s inflater is RFC 1950-only *and* verifies the Adler-32. `gzip/1`/`gunzip/1`
  are the **RFC 1952** pair: a fixed 10-byte header (`1f 8b 08 00` = ID1/ID2, CM=8, FLG=0, MTIME=0, XFL=0, OS=3) +
  the raw DEFLATE payload + the LE CRC-32 of the *original* data + the LE ISIZE (length mod 2^32); `gunzip/1`
  validates the header (reserved `FLG` bits, `FEXTRA`/`FNAME`/`FCOMMENT`, `FHCRC`), verifies both trailers, and
  **loops** so concatenated members concatenate. Every malformed input is the bare-atom `error:data_error` (`<<>>`, a
  bad magic/CM, a reserved `FLG` bit, a corrupted CRC-32/ISIZE, trailing junk, truncation); `badarg` only for
  non-iodata. `unzip/1` is the one function that still carries the `MAX_UNCOMPRESSED` (32 MiB) ceiling. The CRC-32
  is `checksum.rs`'s `crc32_step` (widened to `pub(crate)`, not copied).
The deflate payload is miniz_oxide's, so byte identity with the oracle's stream is **not** promised (the framing,
interop and the round trip are). The stream API (`open`/`deflate`/`inflate`/`close`, `*_init`/`*_reset`) stays
`undef`. No `unsafe`.

### The auxiliary BIF modules
- `checksum.rs` (10): CRC-32/Adler-32 (+ the `*_combine/3` forms) and `md5/1` plus the streaming
  `md5_init/0`/`md5_update/2`/`md5_final/1` — zlib's checksums and `erl_md5.c` transcribed (no new dependency); the MD5
  `pad` boundary is KAT-pinned for every message length `0..=64`/`120..=128` (mod 64) plus RFC 1321.
- `calendar.rs` (+ `calendar/localtime.rs`) (9): the date/time family and the
  `posixtime`⇄`universaltime`⇄`localtime` conversions; the pure arithmetic is safe Rust, **only** the timezone/DST ops
  live in the crate's **second** `#[allow(unsafe_code)]` module (`tzset()` once per process; `daylight` re-read per
  call).
- `os_bif.rs` (13): the `os` env/pid/clock BIFs, `file:native_name_encoding/0` and the pure `prim_file` name helpers.
- `fun_info.rs` (5): `fun_info/1` (**the one fast path here** — `bif.tab` declares only `/2`; the oracle implements
  `/1` in Erlang) is the OTP-29 ten-key walk, sharing `item_value` with `/2` (so the two forms cannot disagree, and
  the `LocalFunIdentity` seam is consulted **once** per call) while dropping every `{Key,undefined}` pair; `/2` also
  gains the OTP-29 `pid` item (seam detail: Known Issues). Plus `fun_info_mfa/1`, `fun_to_list/1` and `make_fun/3`.
  Arity split (oracle-verified): `fun_to_list/1` renders the **true** arity while `fun_info/2`'s `arity` and
  `fun_info_mfa/1` read the header's 8-bit field — both are the C's own behaviour.
- `term_conv.rs` (17): the term-conversion/textual-id BIFs plus `binary:referenced_byte_size/1`; the OTP-30
  `*_to_binary/1` are pinned to the C `"%T"` format (the oracle does not export them); `list_to_ref/1` bounds the id
  at `MAX_REFERENCE = 1<<18`. `{pid,port,ref}_text_str`/`external_channel_gap` are the one homes of the text and of
  the external refusal.
- `erts_internal.rs` (10): `erts_debug:same/2` (raw `Eterm`-word identity), `erts_debug:flat_size/1`, and eight
  `erts_internal` helpers (incl. the real `map_hashmap_children/1` over the HAMT accessor).
- `modinfo.rs`, `persistent_term.rs`, `proc_reg.rs`: the `ModuleInfo`/`PersistentTerm`/`ProcessHook` BIFs; the BIF
  layer owns the rendering and the argument-decided `badarg` cases (a non-atom name, `undefined` as a name, a
  non-internal id, a non-pid, a non-atom flag, a bad item spec, an absent `get/1` key, a `put_new/2` onto a
  conflicting key) and checks them *before* the hook; `get_module_info/2` takes exactly nine item atoms.
- `match_spec.rs` / `packet/`: `erlang:match_spec_test/3` (the `table` dialect, returning the C's
  `{ok,Result,Flags,Errors}`/`{error,Errors}`) and `erlang:decode_packet/3` — `mod.rs` owns the argument validation,
  the 15-type switch, dispatch and rendering, with one family module each (`framing`/`tls`/`http`), so **no valid
  packet type is a typed gap**. `./src/packet/CONTEXT.md`.
### `erlang:raise/3`'s stacktrace forwarding (`convert.rs`)
`raise(Class, Reason, Stacktrace)` **decodes** its valid 3rd argument into the returned raise's
`beam_common::StackTrace` (`decode_stacktrace`/`decode_stacktrace_frame`/`decode_location`, next to the
`is_stacktrace` validity predicate the returned-`badarg` gate uses). The engine renders a non-empty supplied
trace **verbatim** in place of its own raise-snapshot trace (`beam_engine`'s supplied-frame latch), so the two
halves together reproduce the oracle's `build_stacktrace` — a caller-supplied trace comes back unchanged. The
decode mapping:
- `{Module, Function, Arity[, Location]}` → `StackFrame { module, function, arity, location }`; `Module`/
  `Function` are atoms read from the shared table by **name**; the 3rd element is the `arity: u8` **only when it
  is an integer in `0..=255`**; an absent 4th element or `[]` is `location: None` (the `NIL` the engine renders,
  i.e. the same `[]` term the oracle normalizes `{M,F,A}` to).
- `Location` is exactly `[{file, FileNameCharList}, {line, N}]` — those two pairs in that order; `FileName` is a
  **char list** of Unicode scalar values (never an atom) and `N` an integer in `0..=u32::MAX`.
- `[]` (an empty 3rd argument) decodes to the **empty** `StackTrace`, so the engine's latch stays unarmed and
  `raise(_, _, [])` keeps its derived stacktrace.
- The reason handling is orthogonal and unchanged: an atom reason keeps its exact `ExitReason` with no live slot,
  a non-atom one is `ExitReason::Structured` carrying the term itself in the `Raise::live_reason` slot; **both**
  additionally carry the decoded frames.
**Never fabricated.** A trace element the oracle echoes verbatim but a `StackFrame` cannot express is the typed
`unsupported_bif` **raise** — never the returned `badarg` (the argument is valid, so it is a capability gap) and
never a truncated/invented frame. The gap covers: an `erl_eval`-style `{Fun,Args}` / `{Fun,Args,Location}` **call
frame** (no module/function), a 3rd element that is an argument list or any other non-`0..=255` value (`-1`,
`256`, `1.0`), a `Module`/`Function` the atom table cannot name, and a location that is not exactly the shape
above (an atom/other file name, a missing or extra pair, reversed order, a non-integer or out-of-`u32`-range
`line`, a char list element that is not a Unicode scalar value). The returned-`badarg` cases are untouched: a bad
class, and a non-list/improper/non-frame trace (`[42]`, `[{1,2}]`, `[{}]`, `42`, `[{m,f,1}|x]`) or a location that
is not a list.
### The port BIFs (`port.rs`) and the `PortHook` seam
A port is OS process + driver + port-table slot (Stage-5 machinery, never a BIF), so every port *operation* routes
through the optional `PortHook` — `unsupported_bif` with none. `port_close/1` returns `true` (never `ok`);
`port_info/1` emits the oracle's seven items in order and `port_info/2` returns `{Item,Value}` or `badarg` for an
unknown item; `open_port/2` forwards `Name`/`Options` to the hook (which validates the grammar, incl.
`open_port(_, [list])` ⇒ `badarg`); `list_to_port/1`/`port_to_list/1` are pure (`#Port<0.Num>`).
`port_command/2,3` and `port_info/1,2` on a port the runtime does not hold go to `../beam_rt/src/native/port.rs`.
### Fun-taking `lists`/`maps`/`ets` BIFs (the `FunCaller` seam)
The 18 `lists`, 4 `maps` and 2 `ets` (`foldl/3`/`foldr/3`) fun-taking functions delegate to the optional `FunCaller`
(a BIF cannot invoke a fun). The fun argument is validated **exactly as the oracle's clause guards do, before any
list/map inspection**: `lists` raises the bare `error:function_clause` for a non-fun or wrong-arity fun (the
`lists.erl` head), `maps`' raise plain `error:badarg` (the `maps.erl` fallback clause); a **valid** fun with no hook
is the typed `unsupported_bif`. `zipwith/3` raises `error:function_clause` on a length mismatch; `all/2`/`any/2`
short-circuit; `maps:filter/2` treats a non-boolean result as `BadArg`.
### `erlang:term_to_binary/1,2` and `binary_to_term/1,2` (`binconv.rs`)
- **The decode table image is memoized:** `decode_term_bif` decodes into `DECODE_SCRATCH`, a per-thread **image** of
  `ctx.atoms()`, reused only when `prefix_matches` names every index the same atom and then extended to an **exact**
  image, so a novel atom's index equals the `AtomInterner` prediction (`base = shared.len()`); any other table shape
  (moved/shorter, or a scratch left ahead by a failed/`safe` decode) forces a rebuild. `BifContext::atom_and_heap` is
  the one borrow-split route, so **no BIF clones the whole `AtomTable` per call** — `tests/decode_hot_path.rs` pins the
  steady-state per-call allocation count as table-size-**independent** (9 allocations / 602 bytes, identical at 137
  and 4105 atoms). Measured (`examples/etf_bench.rs`): a clone-per-call decode is 130.3 µs/op against the memoized
  **5.45 µs/op (23.9×)**.
- **Options.** `compressed`/`{compressed,0..=9}` really deflate a version-less body and wrap it `[131][80][len][zlib]`
  **only** when the stream fits the C's `body_len - 5` budget (bare atom = level 6); `{minor_version,0|1|2}` selects
  the tag set byte-identically; `iovec`/`fragment` are `badarg`. `[deterministic]` is **fully honoured** (threaded as
  `Encoder::with_deterministic`, so every flat/HAMT/nested map is re-sorted into full term order); `local` stays the one
  typed gap (its `LOCAL_EXT` payload is the per-VM `local_node_hash` — no seam). `[deterministic,local]`/unknown ⇒
  `badarg`; `/3` does not exist (oracle `undef`). `binary_to_term/2` takes `safe`/`used` (the count includes the `131` byte); on a
  top-level `COMPRESSED` image `[used]` is the compressed image's own length (trailing bytes ignored *and* uncounted).
- **The local-node identity is installed on BOTH directions** (`LocalNode`; `(nonode@nohost, 0)` fallback): encoding
  yields a byte-exact `NEW_PID_EXT`/`V4_PORT_EXT`/`NEWER_REFERENCE_EXT`, and decoding demotes such a wire id to the
  INTERNAL form when it fits the M1 slot, so `binary_to_term(term_to_binary(self())) =:= self()` and the `make_ref()`
  form both hold; an id that does not fit keeps the faithful **external** form (byte-exact round-trip).
- **Binary/bitstring payloads round-trip** (`BINARY_EXT`/`BIT_BINARY_EXT`, on-heap `ErlHeapBits` and off-heap refc
  `BinRef`/`ErlSubBits`, at any nesting, under `compressed` and every `minor_version`), so the encoder's
  `unsupported_external_term` is raised only for an internal pid/port/ref with no resolvable local node and for a
  non-Latin-1 atom under the Latin-1 flavour — never for a binary. A runtime-made `Local` fun encodes as its
  `NEW_FUN_EXT` image once `LocalFunIdentity` answers (read *before* the atom-table borrow), else the typed
  `unsupported_local_fun`. `float_to_list/2`/`float_to_binary/2`'s `short` is the `sys_double_to_chars_short` port
  with the oracle's **exact-tie rule** (~738k doubles, 0 mismatches); `{decimals,N}`/`{scientific,N}`/`compact` too.
### `io`/`io_lib` directive set and the printer (`src/io.rs`, `src/io_ts.rs`, `src/print.rs`)
- **Directives:** `~w ~p ~W ~P ~s ~ts ~c ~tc ~n ~~ ~i ~b ~B ~x ~X ~+ ~# ~e ~f ~g`, the `~k`/`~K` map-print-order
  modifiers (`~k` takes the order without consuming an argument, `~K` takes it from the next one), and prefix `-`
  (left adjust), width (`*`), precision (`.`/`*`), pad character and `t`/`l` modifiers. `~+`/`~#` render the base with
  the `Base#` prefix its own base implies, sign first (`~.16#` of `-255` ⇒ `"-16#FF"`), and every integer directive
  renders arbitrary-precision values. `~c` carries the integer's **low byte**, `~tc` the **full code point
  verbatim** (negative, surrogates and bignums included). `badarg`: `~d`, a base outside `2..=36`, too small a
  precision, a wrong arg type/count, unconsumed or trailing arguments, left-adjusted `~-<n>p`, `~P`/`~W` without a
  depth.
- **Format argument:** a list, a binary and an atom are all accepted (`format_items` mirrors
  `io_lib_format:scan/2`+`collect/2`: atom/binary bytes are ordinary format chars, one element per cons cell, an
  integer `0..=255` is a char, a nested list/binary verbatim); `1`/`{}`/`5`/`[1|2]`/`<<1:3>>` are `badarg`. `~s`
  (Latin-1) walks nested chardata like `~ts` (`io_lib:format("~s",[["ab"]])` ⇒ `[["ab"]]`), but `~s` of an atom with
  a code point >255 is `badarg` where `~ts` accepts. `io_lib:printable_list/1` is the oracle's
  proper-list-of-printable-latin1 rule (no unicode arm — `io:printable_range/0` reports `latin1`), and the same rule
  is the `~p`/`~P` printable-string rule.
- **`io_lib:format/2,3` return the oracle's DEEP segment list** (literal characters as individual small ints, the
  "small" controls splicing flat, every other control contributing one sub-list element). `/3` ignores all options
  except `{chars_limit,N}`, implemented for `~w`/`~p`/`~W`/`~P` and **both** string arms (a list or a binary),
  reproducing `control_limited`'s elision (`N < 4` ⇒ `"..."`, else the first `N-3` **grapheme clusters**); a negative
  limit is the unlimited, byte-identical path.
- **The printer.** `src/print.rs` is the leaf renderer (registers no BIFs); `src/print/pretty.rs` +
  `pretty/cind.rs` are the full `io_lib_pretty` port (`build`/`write*`/`pp*`/`cind/*`), and **`~W` deliberately stays
  on the flat path** (the C `control_limited($W,…)` is a flat depth renderer). Floats render through the same
  `format_short` port (`fwrite_g/1` *is* `float_to_list(F,[short])`), atoms by `io_lib:write_atom_as_latin1/1`,
  non-byte-aligned bitstrings as the leading bytes then `Value:K`, binaries by `write_binary/3`'s depth rule; depth
  semantics reproduce `io_lib:write1/4`. Internal pids/ports/refs render; an **external** one is the typed gap; a
  **fun** renders its `erlang:fun_to_list/1` text (`src/fun_info.rs::fun_text_str`, shared with that BIF) —
  `fun Module:Function/Arity` for an `EXPORT_EXT` fun, `#Fun<Module.OldIndex.OldUniq>` for a `NEW_FUN_EXT` fun —
  and only a `make_fun3` (`FunKind::Local`) fun is the typed `unsupported_bif` gap.
- **The per-element container walks are iterative in both paths** (`render_tail_chunk`/`render_tuple_rest_chunk`/
  `render_map_body_chunk`, and the pretty port's `list1`/`tuple1`/`map1`/`pp_tail`/`pp_pairs_tail`) — one native stack
  frame per *container*, not per element, so a 20k-element list/tuple/map renders instead of aborting, with
  byte-identical output. Nesting **depth** is still recursive (Known Issues).
### The `ets` bridge
`bif.tab` declares 51 `ets:*` keys; the crate **registers 59** (51 declared + 8 `ets.erl` fast paths) and **serves
58**; the 1 remaining C-interior key (`ets:internal_request_all/0`) is a seam-backed typed gap in
`src/runtime/other.rs`. `ets:fun2ms/1` and `ets:repair_continuation/2` are pure `ets.erl` functions needing AST
access a `BifFn` lacks, so they stay unregistered. Load-bearing invariants:
- **`select_replace/2` requires a statically key-preserving body** (`beam_ets`'s `db_match_keeps_key` mirror, run
  **before** the table is traversed), so a rejected body is `badarg` *even when the spec matches nothing*, a **valid**
  never-matching spec replaces 0 objects, and a `bag` is `badarg` up front.
- **No returned term aliases table storage** — each BIF *takes* the `beam_ets::Table` out, works on it and restores
  it, and every table-derived term is copied onto the caller's heap (`beam_message::copy_struct_simple`).
- **`ets:info/2`:** `fixed` → `Table::is_fixed()`, `binary` → `[]` for a bitstring-free table, `safe_fixed`/
  `safe_fixed_monotonic_time` → `false` unfixed, `{fixed,true}` creation is `badarg` in this oracle, and `ets:info/1`
  is the oracle's 15 pairs. `stats` is a typed gap (it needs `beam_ets` interior counters — see Handoffs).
- **The match-spec arithmetic hook is installed scoped and depth-counted** (`src/ets/arith_hook.rs`
  `PrecisionArith`, delegating to `crate::arith::match_spec_arith` for arbitrary precision), because `beam_ets`'s
  built-in `DefaultArithHook` is checked small-int/float only. It is live only around a spec evaluation (the ten
  `ets` match-spec BIFs, the `/1` continuation resumes included, plus `erlang:match_spec_test/3`), `INSTALL_DEPTH`
  makes nested guards share one install, and a failed operator maps to the `badarg` class the executor treats as the
  oracle's own raise — the hook never fabricates a result. `tests/match_spec_arith.rs` proves both halves against the
  live oracle. `TableRegistry` (the single table-argument resolver), owner/access gating, the `ets:new/2` option
  parser + `TableProfile` sidecar, the unknown-table `undefined`, the adopted verbs and every remaining typed gap:
  **`./src/ets/CONTEXT.md`**.
## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only, never a crate-local error enum, never `panic!`, never
  `unwrap()` on runtime data, never a wrong result. `BadArg`/`BadArith`/`BadArity`/`SystemLimit` for the
  argument/numeric/arity/size failures; the typed `unsupported_bif` **raise** for a capability gap. A capability
  gap is never reported as `badarg` (the argument is valid) and a `badarg` is never reported as a capability gap.
- **Never re-implement term order/size/hashing/construction.** Comparison ⇒ `beam_term::compare::{cmp, cmp_arith}`;
  hashing ⇒ `beam_term::hashing` (`term_hash`, plus the compatibility-pinned `make_hash`/`make_hash2` — no local
  copy); construction/inspection ⇒ `beam_term` accessors only (via `access.rs`); allocation ⇒ `beam_alloc::Heap`.
- **`unsafe` confinement.** The crate is `#![deny(unsafe_code)]`; the only two `#[allow(unsafe_code)]` modules are
  `src/access.rs` (term/int/float/bignum/bitstring/flatmap/native-record wrappers) and `src/calendar/localtime.rs`
  (the C time-library bridge); `unsafe_op_in_unsafe_fn` stays clean.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `beam_abi`, `beam_process`,
  `beam_message`, `beam_codec`, `beam_ets`, `beam_gc`, `libc`. `beam_abi` is a **new** wave-0 → wave-5 edge, added
  alongside the pre-existing `beam_message → beam_gc`, `beam_dist → beam_alloc` and `beam_engine → beam_alloc`
  edges. **No `beam_loader` edge** — native-record definitions arrive through the `RecordSource` seam. Adding a
  dependency edge requires reporting to `../`.
- **Files target ~1000 lines**; several exceed that only via their in-module `#[cfg(test)]` suites, each
  implementation section staying under ~1300 lines. The printer and the `ets` bridge are **split by
  responsibility**. Licensing Apache-2.0; every `*.rs` carries the OTP `// %CopyrightBegin% … // %CopyrightEnd%`
  Apache-2.0 block.

## Notes for Agents
- **`access.rs` and `bits.rs` are PRIVATE** (`mod access;`/`mod bits;`); the cross-crate public surface is exactly
  the `pub use` list in `src/lib.rs` + `prelude`, and there is **no public term construction/destruction helper
  module**.
- **To destructure/construct boxed terms from safe Rust there are two routes:** (a) the registered BIFs through
  `BifRegistry::call`; or (b) the sibling **safe** helper modules `beam_engine::terms::*` / `beam_codec::term::*`
  (`make_float`/`make_big`/`make_flatmap`/`make_binary`/`make_bitstring`, `flatmap_parts`/`tuple_arity`/
  `tuple_element`/`cons_head`/`cons_tail`/`bitstring_bytes`/`float_value`), both exposing safe `Result` APIs.
- `BifContext` constructs floats, lists, tuples, cons, small ints and atoms safely but has **no**
  binary/bitstring, bignum, map or record constructor — build those via the routes above.
- **Oracle cross-checks back every hard algorithm.** Bare BIF names are **not** auto-imported under `-eval` (fully
  qualify). Several probes need a **compiled module** (`erlc` on a fixture) — e.g. `erts_debug:flat_size/1` and
  `lists:sort/2`'s degenerate comparator. Most "matches the oracle" unit tests are pinned-golden self-checks; the
  integration files that shell out to system `erl` are **fail-loud** (they panic if `erl` is absent or errors).
- **CI:** `.github/workflows/erts_rust.yml` (path-triggered on `erts_rust/**`) builds the workspace
  (`--all-targets`), runs `cargo test --workspace --no-fail-fast`, a non-blocking clippy, and the differential gate
  `../../tools/differential/gate.sh` against an installed Erlang/OTP oracle. A `build --workspace` must stay
  warning-free.
- **Build env:** sandbox `$HOME` is read-only — always run cargo via
  `erts_rust/scripts/cargo.sh … --manifest-path erts_rust/Cargo.toml -p beam_bifs`, never a bare `cargo`.

## Test Strategy
- Per-module `#[cfg(test)]` unit tests build a `BifContext` over a real `beam_alloc::Heap` + a seeded
  `beam_term::AtomTable` with a `BytesSink`/`NullSink` capture.
- `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_bifs --no-fail-fast` runs the whole
  crate green. Re-derive the split from the per-suite `test result` lines, not from a hand-maintained table.
- **Pins.** `tests/roster_audit.rs` is the roster pin (see the census section). `tests/bif_tab.rs` pins the parsed
  `bif.tab` shape and the census (`IMPLEMENTED_DECLARED = 556`, `MISSING_DECLARED = 0`, `FAST_PATHS = 92`,
  `REGISTRY_SIZE = 648`) — it reads the registry, never altering dispatch. `src/runtime/erlang.rs`'s
  `is_builtin_sweeps_the_declared_table_and_the_whole_fast_path_set` pins the same two figures in-crate.
  `tests/registry_hot_path.rs` is a
  per-thread-armed counting `#[global_allocator]` proving `lookup`/`contains`/hit-`call` allocate **0** bytes, plus a
  median/p99 ns/call print with a loose regression tripwire; `tests/decode_hot_path.rs` is the same
  allocation-counting technique applied to `binary_to_term/1`, pinning that its steady-state cost does not scale with
  the shared `AtomTable` (a per-call clone would allocate on the order of the table size).
- **Correctness-by-construction pins.** `tests/decode_scratch.rs` carries the per-thread `DECODE_SCRATCH` image
  across calls, covering every stale-image shape (same table twice, a growing table, a *moved* name index, a shorter
  table, a fresh atom at exactly `shared.len()`, a truncated and a `safe` decode); `tests/binary_to_term_atoms.rs`
  pins the hook level. `src/binconv.rs`'s unit tests cover the local-node identity both ways. `tests/binary_family.rs`
  drives `binary:*` over an **off-heap** refc binary (a 96-byte `write_refc_binary` payload — the `bs_create_bin`
  shape above `ERL_ONHEAP_BINARY_LIMIT`). `tests/match_spec_arith.rs` proves both halves of the arithmetic hook.
- **Oracle-backed suites.** `tests/{io_lib_format_chars_limit,io_lib_format_ts,io_lib_format_ts_limited}.rs` pin the
  limiting `{chars_limit,N}` rendering and the `~ts` matrix against committed goldens **plus** a fail-loud live-oracle
  cross-check; `tests/phash.rs`, `tests/checksum.rs` (incl. the MD5 `pad` KAT boundary), `tests/packet*.rs` and
  `tests/zlib.rs` (the raw-DEFLATE pairs, incl. a level/size sweep that crosses all three block types and a
  `zlib:zip/1` cross-check against the live oracle) do the same. **Still missing:** a committed differential case for
  the `math` domain-error matrix (development-only evidence exists for `math` 32 probes, `checksum` 34,
  `match_spec_test/3` 9, `calendar`/`os_bif` 21).
- **Committed micro-benchmarks** (`--release`; read the printed ratios, absolute latencies are meaningless on the
  shared host, both print `/proc/loadavg` first): `examples/etf_bench.rs` (the `term_to_binary`/`binary_to_term`
  round trip vs. an isolated `AtomTable::clone`) and `src/bin/ets_bench.rs` (the match-spec/`ets` traversal with an
  `ets:info/2` control and an in-run `legacy` whole-table-clone phase).

## Known Issues / typed capability gaps
**No known silent wrong *values*.** Every capability gap returns a typed `BeamError`, and every argument error
matches the oracle's class. Entries marked *divergence* are honest but not byte-identical.
### Divergences
- **`math:erf/1`/`erfc/1` are approximate** (`src/math.rs`): `erf` ≤ 4 ULP, `erfc` up to ~506 ULP — enough to change
  a `~w` shortest render; Rust has no `f64::erf`/`erfc` and glibc fdlibm is not Apache-2.0, so `tools/differential`
  does not pin the pair (it does pin `atanh/1`, which IS bit-exact).
- **`erlang:display/1` writes a printable latin-1 char ≥ `16#80` as UTF-8, not the oracle's raw code unit** (≤ `16#7F`
  is byte-identical; `io:format("~p",…)`/`display_string/2` are unaffected). Closing it needs a byte-oriented
  `OutputSink` (`src/context.rs` + `beam_rt`), not a local fix.
- **A `FunKind::Local` (`make_fun3`) fun is the typed `unsupported_bif` gap** in the printer (and in
  `erlang:fun_to_list/1`, which it shares): its frozen descriptor is only `(uniq, index)`, so the
  module/name a `#Fun<…>` needs is not in the term and any text would be a silently-wrong result. An
  `EXPORT_EXT` fun renders `fun Module:Function/Arity` and a `NEW_FUN_EXT` fun renders
  `#Fun<Module.OldIndex.OldUniq>` — byte-faithfully, at any nesting depth. An internal **port** renders
  (`#Port<0.N>`); an external pid/port/ref is the typed gap below.
- **A non-small-integer `{chars_limit,N}` is rejected up front**, whereas the oracle only fails when arithmetic
  *reaches* the value (`io_lib:format("hello",[],[{chars_limit,4.0}])` ⇒ `"hello"` there, `badarg` here). In-range
  small-integer limits are byte-identical.
- **`get_module_info`'s `exports`** lists loader-order exports, not the oracle's global export-table order (same set,
  order only — accepted at `../beam_rt/src/context/module_info.rs`); every other item matches.
- **`erts_internal:term_type/1` reports `export` for an `Imported` fun** where the oracle reports `fun` (yet
  `fun_info/2`'s `type` reports `local` for the same term).
- **Reject-where-the-oracle-succeeds / class mismatch:** `re:inspect/2` of a non-`namelist` item is the typed gap where
  the oracle raises `badarg`; `lists` of a non-list/improper argument raises `badarg` where the oracle raises
  `function_clause`/`case_clause`/`{bad_generator,_}`. No differential case covers these.
### Typed capability gaps (`unsupported_bif`, never a fabricated value)
- **External (remote-node) pid/port/ref textual ids and rendering:** `pid_to_list`/`ref_to_list`/`port_to_list`, the
  `*_to_binary` forms and the printer's external branch answer the typed gap (`src/term_conv.rs::external_channel_gap`).
  The oracle's channel is `atom_val(x->sysname)` — the node-name atom's index in the **running VM's** atom table,
  state the frozen term does not carry (one `V4_PORT_EXT` image printed `#Port<10643.7>` then `#Port<10639.7>`). Needs a
  node-identity seam **and** a node-atom → dist-channel provider (both `beam_rt`, a new edge — Handoffs).
- **`term_to_binary/2`'s `local`** answers the typed gap (the `LOCAL_EXT` hash is the running VM's per-start random
  state, no seam); `deterministic` is closed (byte-exact). `erlang:external_size/2`'s `local` is the same gap.
- **`erlang:raise/3`'s unrepresentable stacktrace elements** answer the typed gap while the argument is valid: an
  `erl_eval`-style `{Fun,Args}`/`{Fun,Args,Location}` call frame, a 3rd element that is an argument list or any
  other non-`0..=255` value (`-1`, `256`, `1.0`), an unnameable `Module`/`Function` atom, and a location that is
  not exactly `[{file,CharList},{line,Integer}]` (an atom file name, a missing/extra pair, reversed order, a
  non-integer or out-of-`u32`-range line, an out-of-range code point). The oracle echoes every such shape
  verbatim — see the per-module contract above for the exact mapping and the untouched returned-`badarg` cases.
- **Fun identity — `name` (and `fun_info_mfa/1`'s function element) only.** They need the loaded module's lambda
  (`FunT`) name, which **no** seam carries (the oracle returns it for a *loaded* module, `[]` otherwise), so
  `fun_info/1` of a non-external fun also raises. Every other identity item IS real: a `FunKind::Local` fun's
  `module`/`new_uniq`/`index`/`uniq`/`pid` come from the `LocalFunIdentity` seam and `new_index` from the term
  itself; a `FunKind::Imported` fun's from its own descriptor.
  **`phash2/1,2`/`phash/2`** of a `Local` fun and of an internal reference
  (`RefIdentityUnavailable`) are the same gap (the LCA's own `TermError`); a native record hashes with its real
  definition hash. **`erts_debug:flat_size/1`** of an external pid/port/ref is a gap too; every other term is computed.
- **`ets:info(T, stats)`** (needs `beam_ets` interior counters — Handoffs) and **`ets:internal_request_all/0`**.
- **Seam-backed until `beam_rt` installs the hook:** the eleven `proc_reg` BIFs, `get_module_info/1,2`,
  `persistent_term:*`, `erlang:get/0`/`erase/0` (until `DictHook::get_all`/`erase_all`), `list_to_atom`/`binary_to_atom`/
  `binary_to_existing_atom` without an `AtomInterner` (`TermNotFound`), and `erlang:load_nif/2` without the
  `NifLoader` **and** `with_current_module`.
- **The 172 `RuntimeHooks` seam keys: 32 served by `beam_rt`, ~99 unreachable** (re-derive by counting the
  `RuntimeHooks::call` match arms in `beam_rt/src/context/runtime_hooks.rs`); 36 more are *shadowed* by `beam_rt`'s
  native floor (dispatched before the registry) and ~41 are served by it. The gaps are the real work
  (`erts_debug`/`erl_debugger` internals; distribution/tracing/code-loading/spawn/monitor machinery; NFC tables for
  `prim_file:internal_normalize_utf8/1`). Detail: `./src/runtime/CONTEXT.md`.
- **`records:get_definition/2` without a `RecordSource` hook is `badarg`** (the oracle resolves in-module), and
  `records:create/4`'s error precedence differs for `[{1,1},5]` (oracle `{badfield,{{m,r},1}}`, here `badarg`).
- **Integer/text limits:** `list_to_integer`/`binary_to_integer` of an over-long string ⇒ `SystemLimit`
  (> ~1.26M digits); `list_to_integer/1,2` requires byte-sized elements; quoted atoms are not re-escaped and
  non-ASCII output is UTF-8 (ASCII byte-exact). **Sub-binaries are copied, not shared:** `binary:part`/`binary_part`/
  `binary:copy`/`split_binary` return a fresh on-heap binary (no `BinRef`/`SUB_BITS`) and a 1-limb bignum count the
  oracle accepts is rejected, so `binary:referenced_byte_size/1` reports its own byte size (oracle `100000`, here
  `50000`) — a `beam_term::refc` accessor is the missing piece.
- **The M1 internal-reference `id` is a single 60-bit small**, so an id that does not fit is kept external: a
  `NEW_REFERENCE_EXT`/`NEWER_REFERENCE_EXT` image whose third number is `>= 2^28` decodes to the faithful external
  form (a byte-exact round-trip); `list_to_ref/1` accepts only `#Ref<0.0.0.N>`. Widening the slot spans
  `beam_term::boxed` + `beam_codec::ref_wire` + `beam_gc` copy sizing.
- **`match_spec_test/3`'s `trace` dialect** needs a trace-programme runner, and the `table` dialect's
  compile-rejection gap turns the oracle's semantic-lint `{error,[…]}` into a raise — neither fixable here. **A match
  spec with a 0-arity tuple `{}` panics in the sibling `beam_ets` evaluator** (`ets:select/2`, `ets:match_spec_run/2`,
  `ets:match_spec_compile/1`, `match_spec_test/3`; oracle `badarg`) — fix in `../beam_ets/src/select/CONTEXT.md`.
- **Deep *nesting* still recurses in the printer** — an unbounded-*depth* concern; the per-element walks are
  iterative, so a long flat container no longer aborts.
### Environment-bounded values and bounded M1 leaks
- **`ets:all/0` answers only this crate's registry, ascending-id** (the oracle also lists the node's system tables,
  no order) — the only `ets` value divergence.
- **Never-reclaimed registries (bounded M1 leak):** the compiled-pattern, `ets` continuation/compiled-spec and
  `atomics`/`counters` cell registries have no off-heap finalizer yet. The one *stale-word* hazard is the `ets`
  compiled-spec registry: `TableRegistry::insert_spec`/`spec_raw` (`src/ets.rs`) store the caller's spec **`Eterm`
  verbatim** in a non-root registry that outlives the call, so a relocating collection can leave it stale — closing
  it needs table-owned storage (a `beam_rt`/`beam_ets` change, Handoffs). The compiled-pattern registry deep-copies.
- **`time` native units are fixed** (ns `monotonic_time/0` over a process-global `Instant` origin; `unique_integer`
  `>= 0`); `time_unit` accepts the oracle's atom set and a **positive bignum** unit IS supported
  (`crate::time::{convert_unit_big,int_from_i128}`, floor semantics; past `BIG_ARITY_MAX` is `badarg`). **`error/2`
  ignores its second argument**, `nif_error/2`'s is crash-report metadata, `error/3` mirrors `error/2`; **`prim_*`**
  modules are otherwise unimplemented (only the three `os_bif.rs` `prim_file` helpers).
- **The printer cannot emit a `beam_term::ops: class: invalid boxed header subtag …` panic** — that is a
  corrupted/stale-term symptom (a flat-map render with a **non-atom** key, `../beam_rt/M1_GAPS.md`), not a printer
  input: no `src/print.rs`/`src/print/pretty/` path calls `beam_term::ops::class` or panics.

## Design Decisions
- **`BifFn` is a plain `fn` pointer**, `Copy`, allocation-free to register, impossible to close over state (all
  state flows through `BifContext`).
- **`BifContext` holds optional seams, not concrete runtime types** (testable with a bare heap + atom table),
  keeping the Stage-1/Stage-5 DAG acyclic (no `beam_engine` edge).
- **Stateful/global BIFs route through seams** — the crate owns neither global state nor OS machinery (the two
  real, seam-free families are the `atomics`/`counters` cells and the `erl_ddll` driver registry + error table; a
  resource handle is an on-heap `Ref` thing over a module-local registry, `./src/runtime/CONTEXT.md`).
- **`function_clause` is a bare-atom raise**; `create`/`update` recognize the by-name atoms (`is_exported`, `'_'`)
  with a local helper, not fixed `AM_*` indices.
- **Bignums come from `beam_codec::term::make_big`, not `beam_term::thing::write_big`** (the latter needs `unsafe`;
  no cycle); the shared `arith` materializers are the single promotion path for every integer-producing BIF.
- **`self/0` reads `BifContext::self_pid`, not a global** (the engine is a pure state machine); all BIF allocation
  is on `beam_alloc::Heap` (`beam_term::heap::Heap` a test arena only).
- **`BifContext::heap()` is the caller's *destination* heap** — in `beam_rt`, the runtime's **pooled, never-reset
  `Runtime::bif_heaps` arena** (`beam_rt/src/runtime.rs` `take_bif_heap`/`put_bif_heap`, default `1<<24` words)
  **outside** the GC-scanned spaces, so a result built there (`ctx.tuple`/`cons`/`list`, `bits::make_binary`, …) is
  unscanned: the runtime's unscanned-root valve *declines* such a collection, and heap exhaustion surfaces as
  `error:system_limit` (`beam_rt/src/error.rs::enforce_term_heap_limit`). This crate never re-roots or copies down.
- **The fun-taking fast paths publish **no** GC roots, deliberately** (measured): they hold live `Eterm`s in Rust
  locals across `BifContext::call_fun`, but the runtime's collection latch is armed **only** for the slice's own
  machine (`beam_rt::runner::BudgetContext::ensure_heap*`), so every nested run has `gc_armed == false`
  (`beam_rt/src/context/bridges.rs::FunBridge`) **and** the process is `relocation_exempt` for the whole run
  (`RuntimeContext::apply_fun`) — two independent refusals, so **no collection can occur while a `beam_bifs` BIF
  frame is live**. A stale-word hazard is never a BIF local but a **runtime-level store** (Handoffs).
- **The `ets` table handle is an on-heap `Ref` thing** (the M1 `tid()` substitution) for a normal table and the
  **name atom** for a `named_table`; a BIF takes the table out of the registry to release the
  `ctx.atoms()`/`ctx.tables()` borrow, and continuations/compiled specs are `Ref` things over registry keys likewise.
- **A capability gap is a `raise`, an argument error is `BadArg`.** Never conflated: the external-identifier refusal
  is the typed `unsupported_bif` (the argument is valid) while a wrong-type argument stays the oracle's plain
  `badarg` on the same BIF (pinned by `tests/term_conv.rs`).

## Performance Notes (per-BIF-call hot path)
- **The registry lookup is allocation-free:** the nested `module → function → [(arity, BifFn)]` maps hash borrowed
  `&str`s via `Borrow<str>` (no key built) and scan the 1–2 element arity `Vec`; a `beam_rt` dispatch reaches it
  twice (`contains`, then `call`→`lookup`) but allocates nothing.
- **Arithmetic widens even small ints to a heap `Big`:** `erlang:'-'/2` of two small ints does ~4 small `Vec`
  allocations (a `Big::from_i64` per operand, then `sub`/`add`/`neg`) with no checked-`i64` fast path.
- **The `AtomTable` deep clone is copy-on-write** (`beam_rt`'s `CodeServer::atoms_mut`, `codeserver.rs:240`): only a
  call that actually **interns** pays it.
- **`binary_to_term`'s steady-state cost is `prefix_matches`'s Θ(table) walk, not a clone** (`binconv.rs:1027-1033`,
  invoked at `:1048`): the memo is a *hit* (zero allocation) but the **validation** scans every interned atom on
  every decode, so its latency scales with the shared `AtomTable`'s **length** while being independent of the
  payload (`term_to_binary/1` does not pay it — encode makes no table walk — hence the encode/decode asymmetry).
  `beam_term`'s O(1) append-only witness (`AtomTable::snapshot` / `appended_since`, `../beam_term/src/atom.rs`)
  **is** the identity signal this check needs — its own bench names `appended_since` "what a memoized table image
  checks on every decode" — but neither decode site (`binconv.rs:1048`, `../beam_rt/src/nif.rs:514`) adopts it yet,
  so the walk is still Θ(number of interned atoms) per call.
## A.9 blockers (self-hosted build: booting kernel, running lib/compiler)
The `ets` link is closed (`src/ets/resolve.rs::parse_options` accepts the full `ets:new/2` option matrix, so
`application_controller`'s `ets:new(ac_tab, …)` and name-resolved `ets:insert/lookup` work; the normalized
`write_concurrency` lock count lives in the `TableProfile` sidecar). There is **no self-hosted build** yet, and the
`ets:foldl/3`/`foldr/3` blocker is in Handoffs.
- **This crate's `erlang` comparison BIFs are the reference for atom ordering — do not re-audit them.**
  `src/compare.rs:84-121` compares by atom *name* over the runtime's one **shared** `AtomTable`
  (`ctx.atoms()`; `beam_rt/src/context/calls.rs:42-47`), and every BIF on the `sofs`
  `restrict_n/5`/`diff_restrict_n/5` path is C-faithful (`erlang:element/2` `src/term_ops.rs:66-102` vs
  `bif.c:2944-2957`; `lists:{reverse/1,reverse/2,keysort/2,sort/1,usort/1}` `src/lists.rs`). The engine's
  guard/`test` path now resolves through the same namespace via the defaulted
  `ExecutionContext::atoms` seam (`beam_engine/src/interpreter/test.rs`), which `beam_rt` overrides with the
  live table, so a guard and the equivalent `<` BIF order terms identically. A name absent from whatever
  table a caller supplies still makes `beam_term::cmp_atoms` (`beam_term/src/compare.rs:356-363`) fall back to
  raw **atom-index** order (the table-relative-`Atom` limitation `beam_term/CONTEXT.md` records), so any new
  comparison site must be handed the shared table, not a per-module snapshot.

## Handoffs (sibling nodes)
### Required from `beam_rt` (exact locations)
- **batch-52 cross-crate findings (verbose detail): `./HANDOFF_batch52.md`** — `-pa`/`-pz` ordering (`-pz` must be a
  tail, currently merged into the `-pa` head), the `+S Total:Online` online count parsed then discarded, and the
  `process_info/2` position items (`current_function`/`initial_call`/`current_location`/`current_stacktrace`/
  `catchlevel`) as typed gaps for a live module-scheduled process; each with a file:line, a fix shape and a repro,
  plus what is **not** a gap and two sibling doc corrections owed.
- **`ets:foldl/3`/`foldr/3` are unreachable in `beam_rt`:** `beam_rt/src/context/calls.rs` drops the shared
  `TableRegistry` whenever `reentrancy_capable(args)` is true (any fun arg), so the `FunCaller` fast path answers
  `beam_rt: unsupported: bif`. **Required change:** exclude the two `ets` folds (the only `FUN_CALLER_BIFS` members
  that also need `ctx.tables()`) from the drop.
- **The `FunIdentityBridge` snapshot is rebuilt per BIF call:** `beam_rt/src/context/calls.rs:201` calls
  `FunIdentityBridge::new(...)` per registered-BIF call for the four `LOCAL_FUN_IDENTITY_BIFS` (incl.
  `term_to_binary/1,2`), and `new` (`beam_rt/src/context/fun_identity.rs`) walks `code_server.loaded()` building
  `modules: Vec<(Arc<Module>, PathBuf)>` — O(loaded modules) allocations the common `term_to_binary` never reads (the
  consumer is already lazy: `binconv::encode_body` asks `ctx.local_fun_identity(term)` only for a `Local` fun).
  **Required change:** cache the snapshot in `Runtime` behind a generation counter and build the per-call bridge over
  it (`FunIdentityBridge::new_cached(&Arc<Snapshot>, …)`). Re-verify with `copy_bound/etf_roundtrip` in `RESULTS.md`.
- **~99 of the 172 `RUNTIME_HOOKS_BIFS` keys are still unserved** (e.g. `erlang:read_timer/1,2`,
  `beam_rt/src/context/tests.rs:2315-2325`); `beam_rt` serves 32 via `RuntimeHooksBridge` + ~41 via the native floor.
  Registering a key can only turn an `undef` into the honest `unsupported_bif`, never shadow a working result (the
  native floor is dispatched *before* the registry).
- **Two stale near-miss fixtures** at `beam_rt/src/context/tests.rs:666` (`the_send_predicate_selects_only_the_message_sending_bifs`)
  and `:1022` (`SEND_HOOK_BIFS` `HookRoster` `misses`): `erlang:send/3` is now a real `BifFn` in `src/proc.rs` and a
  `SEND_HOOK_BIFS` consumer, so each must name a different near-miss (e.g. `("erlang", "send", 1)`).
- **`beam_rt` must install every hook this crate exposes** (each defaults to an honest typed gap): `RecordSource`
  (`records:get_definition/2`, else `badarg`); `LocalNode` (internal pid/port `term_to_binary` — its
  `(nonode@nohost, 0)` fallback and the `bm`/`ac` pattern tags need atoms `CORE_ATOMS` lacks); `LocalFunIdentity`
  (installed for compiled modules' local funs, NOT `-eval`); `ModuleInfo`; `ProcessHook` (11
  `proc_reg` BIFs); `PersistentTerm`; the defaulted `DictHook::get_all`/`erase_all`; `AtomInterner`/`FunCaller`;
  `NifLoader` + `with_current_module`; `PortHook` via `with_port_hook`; and `fun M:F/A` for
  `wire/export_ext_encode`. `MemoryHook`/`RuntimeHooks` are installed; gate via `needs_*` (never a private copy).
  `MemoryHook::memory` returns `Vec<(&'static str, u64)>` in the oracle order `total, processes, processes_used,
  system, atom, atom_used, binary, code, ets`; `RuntimeHooks::call(heap, atoms, module, function, args)` is one
  `(module, function, arity)` dispatch.
- **A node-identity + channel seam** is the only way to close the external-identifier gap (see Known Issues): the
  runtime's own node name plus a node-name-atom → dist-channel provider, additive to `LocalNode`
  (`local_node() -> Option<(Eterm, u32)>`) or a new seam (a new dependency edge, so reported, not assumed).
- **The A.9 in-machine self-host route's stale word is a runtime-level store, NOT a `beam_bifs` local:** the holder is
  `beam_rt`'s node-global `Runtime::persistent_terms`, the only runtime-level store not routed through
  `Runtime::try_stabilize_escaped` (`persistent_term:put/2` stores the caller's words verbatim, `erl_features` puts
  runtime-built terms during parsing), so `get/1` returns a stale word. Fix: stabilize key/value at `put`/`get`
  inside `beam_rt`; publishing BIF locals via `Registers::extra_roots` would be a no-op and must not be added
  (Design Decisions).
- **`beam_rt`'s `-eval` path does NOT route the bit operators through this crate's registry:** its `eval/mod.rs` does
  `band`/`bor`/`bxor`/`bsl`/`bsr` as **small-int** arithmetic (no negative-shift flip, no bignum promotion), while
  compiled-module code resolves the registered `arith` BIFs and agrees byte-for-byte with `erl`.
- **ETS owner reclamation:** `TableRegistry::handles_owned_by(owner: OwnerId) -> Vec<u64>` (every table owned,
  ascending, **unnamed included**) and `owner_of(pid) -> OwnerId`; a process-exit observer applies
  `beam_ets::Table::owner_died` per table — delete an orphan, hand to a live heir, or deliver
  `{'ETS-TRANSFER', Tid, FromPid, Data}` **only when `heir.data` is `Some`** (`./src/ets/CONTEXT.md`).
- **The `erl_ddll` registry is a *separate* table from `beam_rt`'s port driver registry**
  (`../beam_rt/src/port/driver.rs`), so a driver loaded via `erl_ddll:try_load/3` is invisible to
  `open_port({spawn_driver, Name})` and vice versa; unifying the two is the `beam_rt`-side handoff.
- **`beam_rt`'s `-eval` path serves `erlang:raise/3` itself** (`../beam_rt/src/eval/mod.rs`), which masks the reason
  handling on that route but not from compiled callers; that arm also **drops the 3rd argument** — it neither
  decodes the trace (`Raise::with_stacktrace`) nor runs the `is_stacktrace` validity check, so `-eval`'s
  `raise(_, _, Traces)` never carries frames and never returns the `badarg` atom. Forwarding the trace from
  `-eval` is the `beam_rt`-side handoff; the registry path (compiled callers) is complete.
### Requested from siblings
- **`beam_ets` — interior statistics for `ets:info(T, stats)`.** `TableInfo` has no per-table counters
  (inserts/lookups/deletes), so `ets:info/2`'s `stats` item is the typed gap; an additive `TableInfo::stats()` (or
  `Table::stats()`) returning the oracle's `[{Item, Value}]` shape is the fix.
- **`beam_ets` — the `{}`-in-match-spec panic** (see Known Issues): the one-line index fix belongs in
  `beam_ets/src/select/expr/mod.rs::decode_tuple`.
- **`beam_process` — `erlang:monitor/3` with a non-immediate tag.** The key is registered here but is **shadowed** by
  `beam_rt`'s native floor (`native/process/monitor.rs`), which forwards the tag to
  `ProcessTable::monitor_*_with_tag`; `beam_process::check_tag` refuses a **non-immediate** tag (`{a,b}`, a binary)
  with `error::unsupported_tag`, while the oracle accepts any term and delivers
  `{{a,b}, Ref, process, P, Reason}`. Closing needs `beam_process` to hold a deep-copied tag (a table-owned/
  `TermStore` copy, like `Table::install_heir`) instead of a raw heap `Eterm`.
- **`beam_codec` — `with_deterministic()` is wired** by `binconv::encode_body`; `encode_fun_external` already emits an
  `EXPORT_EXT`'s **true** arity (`fun_true_arity_term`), so no >255-truncation. **Still owed:** a `local_node_hash` seam
  so `beam_rt` can close `[local]` (and `external_size/2`'s inherited gap). Its only other remaining gap is the
  reference-`id` representability one.
- **Differential cases this node's coverage should be pinned by** (corpus owner: `../../tools/differential/`):
  records; wire (self-pid/internal-port `term_to_binary`, `export_ext_encode`); phash/phash2 edge values;
  `binary:compile_pattern` shapes; `short` float rendering; the printer (`~p`/`~P` re-flow, `~W` depth,
  left-adjusted `badarg`); the `ets` select/reverse/match/continuation/spec/option/access family; `lists:usort/2`
  and `lists:sort/2` degenerate comparators; `math:atanh/1`; the `atomics`/`counters` and `erl_ddll:format_error_int/1`
  values; the `unicode` wide-encoding vectors; the checksum/calendar/`os`/`proc_reg`/`persistent_term` goldens.

## Routing Table
- `src/lib.rs` -> crate docs, module wiring, root re-exports, `prelude`, `#![deny(unsafe_code)]`
- `src/registry.rs` -> `BifFn`, `BifRegistry`, `with_core_bifs` (648 keys, `runtime` last), `default_registry`
- `src/hook_sets.rs` -> the thirteen hook-gate sets (244 keys; 240 deduped) + the `needs_*` predicates
- `src/runtime/` (**`./src/runtime/CONTEXT.md`**) -> the 209 runtime-coupled BIFs (incl. the 8 real `re` keys)
- `src/re/` (**`./src/re/CONTEXT.md`**) -> the self-contained regex engine serving the 8 `re:*` BIFs
- `src/bif_tab.rs` -> the `bif.tab` parser + the coverage `Census` (validation only)
- `src/context.rs` -> `BifContext`, the fifteen seams, `atom_and_heap`, the view/sink/`ProcessInfoItem` types
- `src/error.rs` / `src/reason.rs` -> the failure constructors, `raise_*`, `structured_error`; the reason mapping
- `src/access.rs` -> term/int/float/bignum wrappers, bitstring/flatmap/native-record access, the M1 ref thing
- `src/bits.rs` / `src/numeric.rs` -> the on-heap bitstring/binary facade; `parse_integer`, int/float rendering
- `src/arith.rs` -> bignum-promoting arithmetic, the six bit operators, `add_terms`/`int_from_*`/`match_spec_arith`
- `src/bool.rs` / `src/compare.rs` / `src/type_test.rs` -> strict booleans; comparisons + `cmp_term/2` + the legacy
  hashes `phash/2`/`phash2/1,2`; the `is_*` tests
- `src/term_ops.rs` / `src/convert.rs` -> tuple/list/size primitives; the pure conversions (incl. `node/0,1`), `raise/3`
- `src/lists.rs` / `src/maps.rs` -> the list operators + the 45 registered `lists` keys; the 24 `maps` BIFs (flat + HAMT)
- `src/math.rs` / `src/binary.rs` -> the 24 `math` BIFs; the binary/iolist/bitstring BIFs + the pattern registry
- `src/binconv.rs` / `src/records.rs` -> `term_to_binary`/`binary_to_term` + the int/atom conversions; the 8 `records:*`
- `src/checksum.rs` / `src/calendar.rs` / `src/os_bif.rs` -> CRC/Adler/MD5; the 9 calendar/clock BIFs; the 13 `os`/`file`/`prim_file` BIFs
- `src/fun_info.rs` / `src/term_conv.rs` / `src/erts_internal.rs` -> fun introspection (`fun_info/1,2`,
  `fun_info_mfa/1`, `fun_to_list/1`, `make_fun/3`; `fun_info/1` the one fast path); the textual-id BIFs (incl.
  `external_channel_gap`); the `erts_debug`/`erts_internal` helpers
- `src/modinfo.rs` / `src/match_spec.rs` / `src/packet/` (**`./src/packet/CONTEXT.md`**) / `src/proc_reg.rs` /
  `src/persistent_term.rs` -> `get_module_info`; `match_spec_test`; `decode_packet`; the `ProcessHook`/`PersistentTerm` BIFs
- `src/string.rs` / `src/unicode.rs` / `src/unicode/` (**`./src/unicode/CONTEXT.md`**, sub-node `grapheme/`) /
  `src/time.rs` -> `string:list_to_float/1`; the `unicode` conversions (UTF-16/32 codecs, the grapheme-cluster port);
  `time` — `monotonic_time`/`system_time`/`time_offset`/`unique_integer`/`timestamp` **and** the `erlang:convert_time_unit/3` fast path
- `src/zlib.rs` / `src/deflate.rs` -> the six `zlib` fast paths: `compress/1`/`uncompress/1` (RFC 1950 over
  `beam_codec::compress`) and the `zip/1`/`unzip/1` + `gzip/1`/`gunzip/1` pairs over the self-contained RFC 1951 decoder
- `src/proc.rs` / `src/dict.rs` / `src/port.rs` -> `self`/`send`/`!`/`send/3` (the `SendHook` family); the
  `DictHook` BIFs; the `PortHook` BIFs
- `src/ets.rs` / `src/ets/` (**`./src/ets/CONTEXT.md`**) -> the `ets:*` bridge (59 keys) + `arith_hook.rs` (`PrecisionArith`)
- `src/nif.rs` -> `erlang:load_nif/2`
- `src/io.rs` / `src/io_ts.rs` / `src/print.rs` -> the `io`/`io_lib` format engine (the `{chars_limit,N}` limited
  paths, the unicode `~ts` arm); the `~w`/`~p`/`~W`/`~P` leaf renderer with `~k`/`~K`
- `src/print/pretty.rs` / `src/print/pretty/cind.rs` -> the `io_lib_pretty` port; the `cind/7` family
- `tests/` -> 81 integration files / 810 tests (lib suite 830); the roster pin is `tests/roster_audit.rs`,
  `tests/{registry_hot_path,decode_hot_path}.rs` are the allocation-count pins, `tests/term_to_binary_options.rs`
  and `tests/fun_info_whole_list.rs` are the fail-loud oracle suites for `term_to_binary/2`'s write-side options
  and for `fun_info/1`'s whole list + the seam-backed local-fun items, and `tests/fun_info.rs` is the pinned-golden
  fun-introspection suite
- `HANDOFF_batch52.md` -> the batch-52 cross-crate gap report (owner/repro/fix-shape per finding)
- siblings `../beam_engine`/`../beam_process`/`../beam_ets`/`../beam_codec`/`../beam_term`/`../beam_loader`/`../beam_io`
  -> the `call_bif` seam, `ProcessId`/`PortId`/`PortTable`, the `Table`/match-spec engine, the ETF codec, the frozen
  term API, the `Recs` chunk, the poller (read-only)
- `../../erts/emulator/beam/` (`bif.tab`, `bif.c`, `erl_bif_*.c`, `erl_arith.c`/`big.c`, `erl_map.c`, `erl_record.c`,
  `erl_db_*`, `binary.c`, `packet_parser.c`, `erl_md5.c`, `external.c`, `erl_term_hashing.c`, `erl_unicode.c`,
  `erl_time_sup.c`), `../../erts/emulator/zlib/` -> authoritative names/arities/semantics (read-only)

## See Also
- `../CONTEXT.md` -> crate-layer contracts, the `BifRegistry`+`BifFn` seam, the wave-5 dependency position.
- `../../CONTEXT.md` -> workspace contracts (§1 A.7, §2 frozen term layout, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.7 -> the OTP app contracts (preloaded/boot/`erts_internal`/`prim_*`).
- `../beam_rt/` -> the runtime that supplies the concrete `BifContext` and adapts the registry into the engine; `../beam_io/` -> the poller/port machinery the `PortHook` handoff targets; `../beam_ets/` -> the match-spec engine (`match_spec_run`, the `*_continue` forms, `TableInfo`) this bridge drives.
