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
`BifRegistry::with_core_bifs()` = **643 keys** = **556 declared + 87 stdlib fast paths** (exports `bif.tab` does
not declare: `lists` +40 over its 5 declared keys — 18 of them fun-taking — `maps` +12 over its 12, 8 `ets:*`,
`io`/`io_lib`, `unicode`, `binary:referenced_byte_size/1`, the OTP-30 `pid_to_binary`/`ref_to_binary`/
`port_to_binary`, `erlang:memory/0,1`, `erlang:convert_time_unit/3`, `zlib:compress/1`/`uncompress/1`).
Re-derive any split from the registry, never from this prose.
**Declared ≠ registered ≠ adopted.** Of the 556, **346 are unconditional in-crate** and **210 are seam-backed**
(they route through one of the fifteen optional `BifContext` hooks and answer the typed `unsupported_bif` until a
host installs the hook). The dedup **seam union is 240** = the 13 `hook_sets.rs` tables (244 raw keys − the 4
`LOCAL_FUN_IDENTITY ∩ LOCAL_NODE` keys); **30** union members are *undeclared* in `bif.tab`
(`MEMORY_HOOK_BIFS` 2 ∪ `PORT_HOOK_BIFS` 4 ∪ `FUN_CALLER_BIFS` 24). **209** keys are runtime-coupled and live
under `src/runtime/` (per-key roster: `./src/runtime/CONTEXT.md`). There are **no dead registrations**.
`tests/roster_audit.rs` is the pin: it builds the union by iterating the 13 public tables (never hard-coding a
key list) and asserts the partition (556 = 210 + 346), the union size, the per-table declared counts, the
per-module declared counts (25 modules), and that every union member is registered with its `needs_*` predicate
true. It caught a real under-report — `ets:foldl/3`/`foldr/3` reach `ctx.call_fun` (`src/ets/adopt.rs`) but were
missing from `FUN_CALLER_BIFS`.
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
  `names`/`entries`); `with_core_bifs()` (the 643-key roster) = `default_registry()`; `register(...) -> Option<BifFn>`
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
**no third-party regex crate** (workspace licensing is Apache-2.0/MIT only) and no `unsafe`; the compiled pattern's
element 5 is **our own** versioned image, never PCRE2's `#Ref<…>`. `{newline,crlf}` is positional (only the pair
`\r\n` is a newline); `{newline,none}` is `badarg`. Contract + gaps: `./src/re/CONTEXT.md`.
### `records` (native records, `records.rs`) — 8 BIFs
OTP-29 native records; the frozen interior is `beam_term::record` (the LCA); ground truth `erl_record.c`. `create/4`'s
cached definition word is the **real** `beam_term::hashing::compute_record_definition_hash` value (written through
`beam_codec::term::make_record`), and `beam_term::hashing` recomputes that value rather than reading word 2, so
`phash2` is unaffected by the stored word. `update/4` takes a map (flat **or** HAMT — the C has an arm for each),
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
### The auxiliary BIF modules
- `checksum.rs` (10): CRC-32/Adler-32 (+ the `*_combine/3` forms) and `md5/1` plus the streaming
  `md5_init/0`/`md5_update/2`/`md5_final/1` — zlib's checksums and `erl_md5.c` transcribed (no new dependency); the MD5
  `pad` boundary is KAT-pinned for every message length `0..=64`/`120..=128` (mod 64) plus RFC 1321.
- `calendar.rs` (+ `calendar/localtime.rs`) (9): the date/time family and the
  `posixtime`⇄`universaltime`⇄`localtime` conversions; the pure arithmetic is safe Rust, **only** the timezone/DST ops
  live in the crate's **second** `#[allow(unsafe_code)]` module (`tzset()` once per process; `daylight` re-read per
  call).
- `os_bif.rs` (13): the `os` env/pid/clock BIFs, `file:native_name_encoding/0` and the pure `prim_file` name helpers.
- `fun_info.rs` (4): `fun_info/2`, `fun_info_mfa/1`, `fun_to_list/1`, `make_fun/3` (`fun_info/1` is **not** registered
  — C's default arm raises `badarg`). Arity split (oracle-verified): `fun_to_list/1` renders the **true** arity while
  `fun_info/2`'s `arity` and `fun_info_mfa/1` read the header's 8-bit field — both are the C's own behaviour.
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
- **The per-call whole-table clone is gone; the decode table image is memoized.** `beam_codec`'s decoders need a
  `&mut AtomTable` to intern the atoms an image carries, so `decode_term_bif` decodes into `DECODE_SCRATCH`, a
  per-thread **image** of `ctx.atoms()`: the memo is reused only when `prefix_matches` finds every index names the
  same atom (name-for-name, allocation-free) and is then extended to an **exact** image — which is what keeps a
  novel atom's index equal to the `AtomInterner` seam's prediction (`base = shared.len()`) so `beam_rt` commits the
  right index. Any other table shape (a moved or shorter table, a scratch left ahead by a failed or `safe` decode)
  forces a rebuild — correctness by construction, not heuristic. Measured (`examples/etf_bench.rs`): 122.6 µs/op ⇒
  **5.39 µs/op (22.7×)**; the encode+decode round trip **20.5×**. Encoding never clones the table.
  `BifContext::atom_and_heap` is the one borrow-split route for "table **and** destination heap", and **no BIF
  clones the whole `AtomTable` per call**.
- **Options.** `compressed`/`{compressed,0..=9}` really deflate a version-less body and wrap it `[131][80][len][zlib]`
  **only** when the stream fits the C's `body_len - 5` budget (bare atom = level 6); `{minor_version,0|1|2}` selects
  the tag set byte-identically; `iovec`/`fragment` are `badarg`. `local`/`deterministic` are **parsed but mostly a
  typed gap** (`local` needs the node's `LOCAL_EXT` hash — no seam; `deterministic` is a no-op when the stored order
  already *is* deterministic). `binary_to_term/2` takes `safe`/`used` (the count includes the `131` byte); on a
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
  semantics reproduce `io_lib:write1/4`. Internal pids/ports/refs render; an **external** one is the typed gap and a
  **fun** is `badarg`.
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
- **To destructure/construct boxed terms from safe Rust a dependent has two routes:** (a) the registered BIFs
  through `BifRegistry::call`; or (b) the sibling **safe** helper modules `beam_engine::terms::*` and
  `beam_codec::term::*` (`make_float`/`make_big`/`make_flatmap`/`make_binary`/`make_bitstring` +
  `flatmap_parts`/`tuple_arity`/`tuple_element`/`cons_head`/`cons_tail`/`bitstring_bytes`/`float_value`), both
  `#[allow(unsafe_code)]` modules exposing safe `Result` APIs.
- `BifContext` constructs floats, lists, tuples, cons, small ints and atoms safely but has **no**
  binary/bitstring, bignum, map or record constructor — build those via the routes above.
- **Oracle cross-checks back every hard algorithm.** Bare BIF names are **not** auto-imported under `-eval` (fully
  qualify, e.g. `erlang:phash2(...)`). Several oracle probes need a **compiled module** (`erlc` on a fixture), not
  `erl -eval` — e.g. `erts_debug:flat_size/1` and `lists:sort/2`'s degenerate-comparator behaviour. Most "matches
  the oracle" unit tests are pinned-golden self-checks; a handful of integration files shell out to system `erl`
  (those that do are **fail-loud**: they panic when `erl` is absent or errors, instead of passing vacuously).
- **CI:** `.github/workflows/erts_rust.yml` (path-triggered on `erts_rust/**`) builds the workspace
  (`--all-targets`), runs `cargo test --workspace --no-fail-fast`, a non-blocking clippy, and the differential gate
  `../../tools/differential/gate.sh` against an installed Erlang/OTP oracle. A `build --workspace` must stay
  warning-free.
- **Build env:** sandbox `$HOME` is read-only — always run cargo via
  `erts_rust/scripts/cargo.sh … --manifest-path erts_rust/Cargo.toml -p beam_bifs`, never a bare `cargo`.

## Test Strategy
- Per-module `#[cfg(test)]` unit tests build a `BifContext` over a real `beam_alloc::Heap` + a seeded
  `beam_term::AtomTable` with a `BytesSink`/`NullSink` capture.
- `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_bifs --no-fail-fast` runs **80
  suite result lines / 1588 tests** (all green): the lib suite (799), 77 integration files (789), and two empty
  harnesses. Re-derive the split from the per-suite `test result` lines, not from a hand-maintained table.
- **Pins.** `tests/roster_audit.rs` is the roster pin (see the census section). `tests/bif_tab.rs` pins the parsed
  `bif.tab` shape and the census (`IMPLEMENTED_DECLARED = 556`, `MISSING_DECLARED = 0`, `FAST_PATHS = 87`,
  `REGISTRY_SIZE = 643`) — it reads the registry, never altering dispatch. `tests/registry_hot_path.rs` is a
  per-thread-armed counting `#[global_allocator]` proving `lookup`/`contains`/hit-`call` allocate **0** bytes, plus a
  median/p99 ns/call print with a loose regression tripwire.
- **Correctness-by-construction pins.** `tests/decode_scratch.rs` issues several decodes per thread so the
  per-thread `DECODE_SCRATCH` image carries across calls, covering every stale-image shape: the same table twice, a
  growing table (extend), a *moved* name index (rebuild — a blind reuse resolves `foo` to the wrong atom), a shorter
  table (rebuild), a fresh atom at exactly `shared.len()` (the interner's predicted index), and a truncated and a
  `safe` decode (both leave the scratch ahead with nothing committed); `tests/binary_to_term_atoms.rs` pins the hook
  level. `src/binconv.rs`'s unit tests cover the local-node identity both ways (top-level/nested/`COMPRESSED`
  demotion, ids `0`…`1<<50`, and the no-identity control). `tests/binary_family.rs` drives the whole `binary:*`
  family over an **off-heap** refc binary it builds itself (a 96-byte payload behind `write_refc_binary` — the shape
  `bs_create_bin` makes above `ERL_ONHEAP_BINARY_LIMIT`; an on-heap literal would hide a byte-reader regression).
  `tests/match_spec_arith.rs` proves both halves of the arithmetic hook (the default hook refuses what the installed
  one computes; the live oracle agrees).
- **Oracle-backed suites.** `tests/{io_lib_format_chars_limit,io_lib_format_ts,io_lib_format_ts_limited}.rs` pin the
  limiting `{chars_limit,N}` rendering and the `~ts` matrix against committed goldens **plus** a fail-loud live-oracle
  cross-check; `tests/phash.rs`, `tests/checksum.rs` (incl. the MD5 `pad` KAT boundary) and `tests/packet*.rs` do the
  same. Most other "matches the oracle" tests are pinned-golden self-checks. Integration files that shell out to `erl`
  are **fail-loud** (they panic when `erl` is absent or errors, instead of passing vacuously). **Still missing:** a
  dedicated `math` domain-error-matrix file (differential evidence for `math` (32 probes), `checksum` (34),
  `match_spec_test/3` (9), `calendar`/`os_bif` (21) was gathered during development but is not yet committed).
- **Committed micro-benchmarks** (`--release`; read the printed ratios, absolute latencies are meaningless on the
  shared host, both print `/proc/loadavg` first): `examples/etf_bench.rs` (the `term_to_binary`/`binary_to_term`
  round trip vs. an isolated `AtomTable::clone`) and `src/bin/ets_bench.rs` (the match-spec/`ets` traversal with an
  `ets:info/2` control and an in-run `legacy` whole-table-clone phase).

## Known Issues / typed capability gaps
**No known silent wrong *values*.** Every capability gap returns a typed `BeamError`, and every argument error
matches the oracle's class. Entries marked *divergence* are honest but not byte-identical.
### Divergences
- **`math:erf/1`/`erfc/1` are approximate** (`src/math.rs`): `erf` ≤ 4 ULP, `erfc` up to ~506 ULP (19,196 of 27,000
  points in `[0,27)` exceed 8 ULP; `erfc(22.658)` is 116 ULP low) — enough to change a `~w` shortest render. Rust
  `f64` has no `erf`/`erfc` and glibc's fdlibm algorithm is not Apache-2.0. `tools/differential` deliberately does not
  pin the pair. `atanh/1` IS bit-exact.
- **`erlang:display/1` writes a printable latin-1 char ≥ `16#80` as UTF-8, not the oracle's raw code unit**; ≤ `16#7F`
  is byte-identical and `io:format("~p",…)`/`display_string/2` are unaffected (`io_lib` really does write UTF-8).
  Closing it needs a byte-oriented `OutputSink` (`src/context.rs` + `beam_rt`), not a local fix.
- **`~w`/`~p` of a fun is `badarg`** (the oracle renders `#Fun<…>`): a `Local` (`make_fun3`) fun carries no
  module/name in the frozen descriptor, so any in-crate value could mismatch. An internal **port** renders
  (`#Port<0.N>`); an external pid/port/ref is the typed gap below.
- **A non-small-integer `{chars_limit,N}` is rejected up front**, whereas the oracle only fails when arithmetic
  *reaches* the value (`io_lib:format("hello",[],[{chars_limit,4.0}])` ⇒ `"hello"` there, `badarg` here). In-range
  small-integer limits are byte-identical.
- **`get_module_info`'s `exports`** lists loader-order exports, not the oracle's global export-table order (same set,
  order only — accepted at `../beam_rt/src/context/module_info.rs`); every other item matches.
- **`erts_internal:term_type/1` reports `export` for an `Imported` fun** where the oracle reports `fun` (yet
  `fun_info/2`'s `type` reports `local` for the same term).
- **Reject-where-the-oracle-succeeds / class mismatch:** `re:inspect/2` of a non-`namelist` item is the typed gap
  where the oracle raises `badarg` (a one-line fix, not done); `lists` of a non-list/improper argument raises
  `badarg` where the oracle raises `function_clause`/`case_clause`/`{bad_generator,_}`. No differential case covers
  these.
### Typed capability gaps (`unsupported_bif`, never a fabricated value)
- **External (remote-node) pid/port/ref textual ids and rendering:** `pid_to_list`/`ref_to_list`/`port_to_list`, the
  `*_to_binary` forms and the printer's external branch all answer the typed gap (`src/term_conv.rs::
  external_channel_gap` is the one home). The oracle's channel is `atom_val(x->sysname)` — the node-name atom's index
  in the **running VM's** atom table, runtime state the frozen term does not carry (the same `V4_PORT_EXT` image
  printed `#Port<10643.7>` then `#Port<10639.7>` in two runs). Needs a node-identity seam **and** a node-atom →
  dist-channel provider (both `beam_rt`-level, a new dependency edge — Handoffs). Real caller: clustered
  EMQX/Khepri rendering remote ids.
- **`term_to_binary/2`'s `local`/`deterministic`** answer the typed gap when they would change the image.
- **Fun identity:** `fun_info/2`'s `name` and `fun_info_mfa/1`'s function element need the module's fun table (no
  seam provides it: the oracle returns the real lambda name for a *loaded* module and `[]` otherwise, so an in-crate
  value can mismatch). A `FunKind::Local` fun's descriptor is only `(uniq:u64, index:u32)`, so **none** of its six
  identity items is derivable; a `FunKind::Imported` fun's items ARE real. **`phash2/1,2`/`phash/2`** of a `Local` fun
  is the same gap (the refusal is the LCA's own `TermError`), as is an internal reference
  (`RefIdentityUnavailable`); a native record hashes with its real definition hash. **`erts_debug:flat_size/1`** of
  an external pid/port/ref is a gap too; every other term is computed.
- **`ets:info(T, stats)`** (needs `beam_ets` interior counters — Handoffs) and **`ets:internal_request_all/0`**.
- **Seam-backed until `beam_rt` installs the hook:** the eleven `proc_reg` BIFs, `get_module_info/1,2`,
  `persistent_term:*`, `erlang:get/0`/`erase/0` (until `DictHook::get_all`/`erase_all`),
  `list_to_atom`/`binary_to_atom`/`binary_to_existing_atom` without an `AtomInterner` (`TermNotFound`), and
  `erlang:load_nif/2` without the `NifLoader` **and** `with_current_module`.
- **The 172 `RuntimeHooks` seam keys: 32 served by `beam_rt`, ~99 unreachable** (re-derive by counting the match arms
  in `beam_rt/src/context/runtime_hooks.rs`); 36 more are *shadowed* by `beam_rt`'s native floor (dispatched before
  the registry) and ~41 are served by that floor. The capabilities behind the gaps are the real work
  (`erts_debug`/`erl_debugger` internals; `beam_process`/`beam_rt` distribution/tracing/code-loading/spawn/monitor
  machinery; NFC tables for `prim_file:internal_normalize_utf8/1`). Detail: `./src/runtime/CONTEXT.md`.
- **`records:get_definition/2` without a `RecordSource` hook is `badarg`** (the oracle resolves in-module), and
  `records:create/4`'s error precedence differs for `[{1,1},5]` (oracle `{badfield,{{m,r},1}}`, here `badarg`).
- **Integer/text limits:** `list_to_integer`/`binary_to_integer` of an over-long string ⇒ `SystemLimit`
  (> ~1.26M digits); `list_to_integer/1,2` requires byte-sized elements; quoted atoms are not re-escaped and
  non-ASCII output is UTF-8 (ASCII byte-exact). **Sub-binaries are copied, not shared:**
  `binary:part`/`binary_part`/`binary:copy`/`split_binary` return a fresh on-heap binary (no `BinRef`/`SUB_BITS`),
  `binary:copy/2` cannot outgrow the destination heap, the endian is `big`/`little` only, and a 1-limb bignum count
  the oracle accepts is rejected. So `binary:referenced_byte_size/1` reports its argument's own byte size, not the
  referenced whole (oracle `100000`, here `50000`) — a `beam_term::refc` accessor is the missing piece.
- **The M1 internal-reference `id` is a single 60-bit small**, so an id that does not fit is kept external: a
  `NEW_REFERENCE_EXT`/`NEWER_REFERENCE_EXT` image whose third number is `>= 2^28` decodes to the faithful external
  form (a byte-exact round-trip), never a truncated id; `list_to_ref/1` accepts only `#Ref<0.0.0.N>`. Widening the
  slot spans `beam_term::boxed` + `beam_codec::ref_wire` + `beam_gc` copy sizing.
- **`match_spec_test/3`'s `trace` dialect** needs a trace-programme runner, and the `table` dialect's
  compile-rejection gap turns the oracle's semantic-lint `{error,[…]}` into a raise — neither is a `beam_bifs`-only
  change. **A match spec with a 0-arity tuple `{}` panics in the sibling `beam_ets` evaluator** (reached through
  `ets:select/2`, `ets:match_spec_run/2`, `ets:match_spec_compile/1`, `match_spec_test/3`; the oracle raises
  `badarg`) — root cause + fix in `../beam_ets/src/select/CONTEXT.md`, not fixable here.
- **Deep *nesting* still recurses in the printer** — an unbounded-*depth* concern; the per-element walks are
  iterative, so a long flat container no longer aborts.
### Environment-bounded values and bounded M1 leaks
- **`ets:all/0` answers only this crate's registry, ascending-id** (the oracle also lists the node's system tables,
  no order) — the only `ets` value divergence.
- **Never-reclaimed registries (bounded M1 leak):** the compiled-pattern, `ets` continuation/compiled-spec and
  `atomics`/`counters` cell registries have no off-heap finalizer for a resource object yet. **The one *stale-word*
  hazard is the `ets` compiled-spec registry:** `TableRegistry::insert_spec`/`spec_raw` (`src/ets.rs`) store the
  caller's spec **`Eterm` verbatim** in a non-root registry that outlives the call, so a relocating collection can
  leave it pointing into the abandoned space; closing it needs table-owned/non-moving storage (or stabilization at
  insert) — a `beam_rt`/`beam_ets`-shaped change (Handoffs). The compiled-pattern registry deep-copies bytes and
  holds no term.
- **`time` native units are fixed** (nanosecond `monotonic_time/0` over a process-global `Instant` origin;
  `unique_integer` values `>= 0`). `time_unit` accepts the oracle's atom set; a **positive bignum** unit IS supported
  (`crate::time::{convert_unit_big,int_from_i128}`; floor semantics; an integer unit past `BIG_ARITY_MAX` is
  `badarg`). **`error/2` ignores its second argument**; `nif_error/2`'s second argument is crash-report metadata, not
  the reason; `error/3` mirrors `error/2`. **`prim_*` modules are otherwise unimplemented** (only the three
  `os_bif.rs` `prim_file` helpers are registered).
- **The printer cannot emit a `beam_term::ops: class: invalid boxed header subtag …` panic** — that signature is a
  corrupted/stale-term symptom, not a printer input: no production path in `src/print.rs`/`src/print/pretty/` calls
  `beam_term::ops::class` (nor `unwrap`/`expect`/`panic!`/an unchecked index), and the only route is a flat-map
  render with a **non-atom** key, i.e. a stale-address read (`../beam_rt/M1_GAPS.md`). Verified:
  `io:format("~p~n",[epp:parse_file(Path,[],[])])` on the ordinary `beam_rt` `-eval` route renders the real `epp`
  output and exits 0.

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
- **`BifContext::heap()` is the caller's *destination* heap, and a term a BIF returns is built there.** In `beam_rt`
  that heap is the runtime's **pooled, never-reset `Runtime::bif_heaps` arena** (`beam_rt/src/runtime.rs`
  `take_bif_heap`/`put_bif_heap`, sized `config.heap_words`, default `1<<24` words), **outside** the calling
  process's GC-scanned young/mature spaces (`beam_alloc::Heap` is a fixed-capacity bump arena whose `reset()` nothing
  calls for a BIF heap). So `ctx.tuple`/`ctx.cons`/`ctx.list`/`ctx.make_float`/`bits::make_binary` (and thus
  `list_to_tuple`, `list_to_binary`, `lists:reverse`, `iolist_to_binary`, …) place their result on a region the
  process collector does **not** scan; the runtime's unscanned-root valve then *declines* any collection whose root
  set reaches such a container, and a BIF-heap exhaustion surfaces as `error:system_limit`
  (`beam_rt/src/error.rs::enforce_term_heap_limit`). This crate never re-roots or copies a result down.
- **The fun-taking fast paths publish **no** GC roots, deliberately** (measured, not assumed). `lists`'
  `map`/`foreach`/`filter`/`foldl`/`foldr`/`all`/`any`/`takewhile`/`dropwhile`/`partition`/`splitwith`/`mapfoldl`/
  `mapfoldr`/`zipwith`/`sort`/`usort`/`keymap`, `maps:map/fold/filter/foreach` and `ets:foldl/3`/`foldr/3` hold live
  `Eterm`s in Rust locals (or a `Vec<Eterm>` accumulator) across `BifContext::call_fun` and must **not** publish
  them as process roots: the runtime's collection latch is armed **only** for the slice's own machine
  (`beam_rt::runner::BudgetContext::ensure_heap*`), so every nested run builds a fresh `RuntimeContext` with
  `gc_armed == false` (`beam_rt/src/context/bridges.rs::FunBridge`) **and** the process is `relocation_exempt` for
  the whole run (`RuntimeContext::apply_fun` → `with_relocation_exempt`) — two independent refusals, so **no
  collection can occur while a `beam_bifs` BIF frame is live**. A stale-word hazard is therefore never a BIF local;
  it is a **runtime-level store** that keeps a caller-heap word without stabilizing it (Handoffs → `persistent_term`).
- **The `ets` table handle is an on-heap `Ref` thing** (the M1 `tid()` substitution) for a normal table and the
  **name atom** for a `named_table`; a BIF takes the table out of the registry to release the
  `ctx.atoms()`/`ctx.tables()` borrow, and continuations/compiled specs are `Ref` things over registry keys likewise.
- **A capability gap is a `raise`, an argument error is `BadArg`.** The two are never conflated: the external
  identifier refusal is a typed `unsupported_bif` (the argument is valid), while a wrong-type argument stays the
  oracle's plain `badarg` on the same BIF. Pinned by `tests/term_conv.rs`'s positive control.

## Performance Notes (per-BIF-call hot path)
- **The registry lookup is allocation-free.** The nested `module → function → [(arity, BifFn)]` maps hash borrowed
  `&str`s (through `Borrow<str>`; no key is ever built) and match the arity by a linear scan of a 1–2 element `Vec`;
  a full `beam_rt` dispatch reaches it twice (`contains`, then `call`→`lookup`) but allocates **nothing**.
- **Arithmetic widens even small ints to a heap `Big`.** `number` builds `Big::from_i64(...)` (a `Vec<u64>`) per
  small-int operand, and `Big::sub`/`add`/`neg` allocate further `Vec<u64>`s: `erlang:'-'/2` of two small ints does
  ~4 small `Vec` allocations with no checked-`i64` fast path.
- **Cross-node:** the shared `AtomTable` copy-on-write clone belongs to `beam_rt`. `RuntimeContext::call_registered`
  holds an `Rc<AtomTable>` handle across the call and commits the names a call interned via `Rc::make_mut`
  (`beam_rt/src/codeserver.rs`), guarded by an emptiness check, so only a call that actually **interns** a name pays
  the deep clone.

## A.9 blockers (self-hosted build: booting kernel, running lib/compiler)
This crate's link in the A.9 chain; the live status is `../CONTEXT.md` (A.9) and `../beam_rt/CONTEXT.md`.
**The `ets` link is closed:** `src/ets/resolve.rs::parse_options` accepts the full `ets:new/2` option matrix (the
bare kind/access atoms plus `named_table`, `compressed`, `{keypos,N>0}`, `{read_concurrency,Bool}`,
`{write_concurrency,Bool|auto|{debug_hash_fixed_number_of_locks,N}}`, `{heir,…}`, `{decentralized_counters,Bool}`),
so `application_controller`'s `ets:new(ac_tab, …)` and every name-resolved `ets:insert/lookup` work, and the
normalized `write_concurrency` lock count — inexpressible in `TableInfo`'s `bool` — lives in the `TableProfile`
sidecar. There is **no self-hosted build** yet; the `ets:foldl/3`/`foldr/3` blocker is in Handoffs.

## Handoffs (sibling nodes)
### Required from `beam_rt` (exact locations)
- **`ets:foldl/3`/`foldr/3` are unreachable in `beam_rt`.** `beam_rt/src/context/calls.rs` drops the shared
  `TableRegistry` from the context whenever `reentrancy_capable(args)` is true (any fun argument), so
  `ets:foldl(Fun, Acc, T)` — this crate's real `FunCaller` fast path — answers `beam_rt: unsupported: bif` while the
  oracle prints the fold. **Required change:** keep the table handle installed for the two `ets` folds (they are the
  only `FUN_CALLER_BIFS` members that also need `ctx.tables()`), i.e. exclude them from the `reentrancy_capable`
  drop or install the registry alongside the `FunCaller`.
- **The `FunIdentityBridge` snapshot is rebuilt per BIF call.** `beam_rt/src/context/calls.rs:201` does
  `FunIdentityBridge::new(&self.runtime.code_server, self.current_module.as_deref(), local_node.node)` **per
  registered-BIF call** for the four `LOCAL_FUN_IDENTITY_BIFS` (incl. `erlang:term_to_binary/1,2`), and
  `FunIdentityBridge::new` (`beam_rt/src/context/fun_identity.rs`) walks `code_server.loaded()` building
  `modules: Vec<(Arc<Module>, PathBuf)>` — `O(loaded modules)` `String`/`PathBuf` allocations per call that the
  common `term_to_binary` (no local fun) never reads. The **consumer** side is already lazy (`binconv::encode_body`
  calls `ctx.local_fun_identity(term)` only when it actually meets a runtime-made `Local` (`make_fun3`) fun), so no
  seam change is needed. **Required change:** cache the snapshot in `Runtime` behind a generation counter bumped when
  the loaded-module set or a module's `LocalFunIdentity` data changes (the code server already knows both events),
  and build the per-call bridge over that cached `&[(Arc<Module>, PathBuf)]`
  (e.g. `FunIdentityBridge::new_cached(&Arc<Snapshot>, current_module, node)` or a
  `Runtime::fun_identity_snapshot()` accessor). Re-verify with the `copy_bound/etf_roundtrip` rows in
  `../tools/bench/RESULTS.md`.
- **~99 of the 172 `RUNTIME_HOOKS_BIFS` keys are still unserved** (pinned example: `erlang:read_timer/1,2`,
  `beam_rt/src/context/tests.rs:2315-2325`). `beam_rt` serves 32 through `RuntimeHooksBridge`
  (`beam_rt/src/context/runtime_hooks.rs`) plus ~41 through the native floor. Re-derive the live figure by counting
  the `RuntimeHooks::call` match arms. Registering a key can only turn an `undef` into the honest
  `unsupported_bif`, never shadow a working result — the native floor is dispatched *before* the registry.
- **Two stale near-miss fixtures** at `beam_rt/src/context/tests.rs:666`
  (`the_send_predicate_selects_only_the_message_sending_bifs`) and `:1022` (the `SEND_HOOK_BIFS` `HookRoster`
  `misses`): `erlang:send/3` is now a real `BifFn` in `src/proc.rs` and a `SEND_HOOK_BIFS` consumer, so each fixture
  must name a different near-miss (e.g. `("erlang", "send", 1)`).
- **`beam_rt` must install every hook this crate exposes** (each defaults to an honest typed gap): `RecordSource`
  for `records:get_definition/2` (else `badarg`); `LocalNode` for internal pid/port `term_to_binary` (its
  `(nonode@nohost, 0)` fallback and the `bm`/`ac` pattern tags need atoms `beam_rt`'s `CORE_ATOMS` lacks);
  `LocalFunIdentity` (installed, so a **compiled module's** local fun encodes byte-faithfully — it does NOT close
  the `-eval` case); `ModuleInfo`; `ProcessHook` for the eleven `proc_reg` BIFs; `PersistentTerm` for the seven
  `persistent_term` BIFs; the defaulted `DictHook::get_all`/`erase_all` for `erlang:get/0`/`erase/0`;
  `AtomInterner`/`FunCaller`; `NifLoader` + `with_current_module` for `load_nif/2`; `PortHook` via `with_port_hook`
  (open the OS process through the spawn driver, allocate with `beam_process::PortTable::alloc()`, register with the
  `beam_io` poller); and it must build `fun M:F/A` for `wire/export_ext_encode`. `beam_rt` installs both
  `MemoryHook` (so `erlang:memory/0,1` are served) and `RuntimeHooks` today. Gate them through
  `needs_memory_hook`/`needs_runtime_hooks`/`needs_local_fun_identity`, never a private copy:
  `beam_rt`'s `RuntimeContext::uses_*` should delegate to `beam_bifs::needs_*` rather than keeping private copies.
  `MemoryHook::memory` returns `Vec<(&'static str, u64)>` in the oracle order
  `total, processes, processes_used, system, atom, atom_used, binary, code, ets`;
  `RuntimeHooks::call(heap, atoms, module, function, args)` is one `(module, function, arity)` dispatch — a partial
  adopter matches the keys it implements and returns the typed `unsupported_bif` for the rest.
- **A node-identity + channel seam** is the only way to close the external-identifier gap (see Known Issues). Two
  pieces: the runtime's own node name, and a node-name-atom → dist-channel provider. Additive to `LocalNode`
  (`local_node() -> Option<(Eterm, u32)>`) or a new seam; it would be a new dependency edge, so it is reported
  rather than assumed.
- **The A.9 in-machine self-host route's stale word is a runtime-level store, NOT a `beam_bifs` local.** The holder
  is `beam_rt`'s node-global `Runtime::persistent_terms` store — the only runtime-level store not routed through
  `Runtime::try_stabilize_escaped`. `persistent_term:put/2` stores `beam_bifs`'s argument words verbatim
  (`PersistentTermBridge::put`, `../beam_rt/src/context/persistent_term.rs`), the `erl_features` module puts
  runtime-built spec/enabled/keyword terms during parsing, a collection at the slice machine's own door then
  abandons them, and `persistent_term:get/1` returns the stale word verbatim into a register — which is why the
  door's root-set audit is silent. Fix shape: stabilize the key/value at `put` (and make `get` return a word the
  caller can hold) inside `beam_rt`; no `beam_bifs` seam is involved. Publishing a BIF's locals via
  `beam_process::Registers::extra_roots` for the duration of `call_fun` would be a structural no-op and must not be
  added (see Design Decisions).
- **`beam_rt`'s `-eval` path does NOT route the bit operators through this crate's registry**: its `eval/mod.rs`
  implements `band`/`bor`/`bxor`/`bsl`/`bsr` as **small-int** arithmetic (no negative-shift flip, no bignum
  promotion), while compiled-module code resolves the registered `arith` BIFs and agrees byte-for-byte with `erl`.
- **ETS owner reclamation:** `TableRegistry` exposes `handles_owned_by(&self, owner: OwnerId) -> Vec<u64>` (every
  table whose `Table::owner()` is `owner`, ascending by handle, **unnamed tables included**) and
  `TableRegistry::owner_of(pid: ProcessId) -> OwnerId`. A process-exit observer lists the dead pid's handles and,
  per table, applies `beam_ets::Table::owner_died` semantics — delete an orphaned table, hand the table to a live
  heir, or deliver `{'ETS-TRANSFER', Tid, FromPid, Data}` **only when `heir.data` is `Some`**.
  `./src/ets/CONTEXT.md`.
- **The `erl_ddll` registry is a *separate* table from `beam_rt`'s port driver registry**
  (`../beam_rt/src/port/driver.rs`), so a driver loaded via `erl_ddll:try_load/3` is invisible to
  `open_port({spawn_driver, Name})` and vice versa; unifying the two is the `beam_rt`-side handoff.
- **`beam_rt`'s `-eval` path serves `erlang:raise/3` itself**, which masks the (now-fixed) reason handling on that
  route but not from compiled callers.
### Requested from siblings
- **`beam_ets` — interior statistics for `ets:info(T, stats)`.** The `TableInfo` surface has no counters
  (inserts/lookups/deletes per table), so `ets:info/2`'s `stats` item is the typed gap. An additive
  `TableInfo::stats()` (or a `Table::stats()` accessor) returning the oracle's `[{Item, Value}]` shape is the fix;
  nothing in `beam_bifs` can synthesize it.
- **`beam_ets` — the `{}`-in-match-spec panic** (see Known Issues): the one-line index fix belongs in
  `beam_ets/src/select/expr/mod.rs::decode_tuple`.
- **`beam_process` — `erlang:monitor/3` with a non-immediate tag.** The key is registered here but is **shadowed** by
  `beam_rt`'s native floor (`native/process/monitor.rs`), which forwards the tag to
  `ProcessTable::monitor_*_with_tag`; `beam_process::check_tag` refuses a **non-immediate** tag (`{a,b}`, a binary)
  with `error::unsupported_tag`, while the oracle accepts any term and delivers
  `{{a,b}, Ref, process, P, Reason}`. Closing needs `beam_process` to hold a deep-copied tag (a table-owned/
  `TermStore` copy, like `Table::install_heir`) instead of a raw heap `Eterm`.
- **`beam_codec` — a `with_deterministic()` encoder mode** would close `term_to_binary/2`'s `deterministic` gap.
  (`encode_fun_external` already emits an `EXPORT_EXT`'s **true** arity via `fun_true_arity_term`, so no
  >255-truncation remains — an arity-256 fun survives byte-faithfully.) `beam_codec`'s local-node decode demotion is
  landed, so no handoff remains there beyond the reference-`id` representability gap.
- **Differential cases this node's coverage should be pinned by** (corpus owner: `../../tools/differential/`):
  records; wire (self-pid/internal-port `term_to_binary`, `export_ext_encode`); phash/phash2 edge values;
  `binary:compile_pattern` shapes; `short` float rendering; the printer (`~p`/`~P` re-flow, `~W` depth,
  left-adjusted `badarg`); the `ets` select/reverse/match/continuation/spec/option/access family; `lists:usort/2`
  and `lists:sort/2` degenerate comparators; `math:atanh/1`; the `atomics`/`counters` and `erl_ddll:format_error_int/1`
  values; the `unicode` wide-encoding vectors; the checksum/calendar/`os`/`proc_reg`/`persistent_term` goldens.

## Routing Table
- `src/lib.rs` -> crate docs, module wiring, root re-exports, `prelude`, `#![deny(unsafe_code)]`
- `src/registry.rs` -> `BifFn`, `BifRegistry`, `with_core_bifs` (643 keys, `runtime` last), `default_registry`
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
- `src/fun_info.rs` / `src/term_conv.rs` / `src/erts_internal.rs` -> fun introspection; the textual-id BIFs (incl.
  `external_channel_gap`); the `erts_debug`/`erts_internal` helpers
- `src/modinfo.rs` / `src/match_spec.rs` / `src/packet/` (**`./src/packet/CONTEXT.md`**) / `src/proc_reg.rs` /
  `src/persistent_term.rs` -> `get_module_info`; `match_spec_test`; `decode_packet`; the `ProcessHook`/`PersistentTerm` BIFs
- `src/string.rs` / `src/unicode.rs` / `src/unicode/` (**`./src/unicode/CONTEXT.md`**, sub-node `grapheme/`) /
  `src/time.rs` -> `string:list_to_float/1`; the `unicode` conversions (UTF-16/32 codecs, the grapheme-cluster port);
  `time` — `monotonic_time`/`system_time`/`time_offset`/`unique_integer`/`timestamp` **and** the `erlang:convert_time_unit/3` fast path
- `src/zlib.rs` -> the `zlib:compress/1` / `zlib:uncompress/1` stdlib fast paths over `beam_codec::compress` (RFC 1950)
- `src/proc.rs` / `src/dict.rs` / `src/port.rs` -> `self`/`send`/`!`/`send/3` (the `SendHook` family); the
  `DictHook` BIFs; the `PortHook` BIFs
- `src/ets.rs` / `src/ets/` (**`./src/ets/CONTEXT.md`**) -> the `ets:*` bridge (59 keys) + `arith_hook.rs` (`PrecisionArith`)
- `src/nif.rs` -> `erlang:load_nif/2`
- `src/io.rs` / `src/io_ts.rs` / `src/print.rs` -> the `io`/`io_lib` format engine (the `{chars_limit,N}` limited
  paths, the unicode `~ts` arm); the `~w`/`~p`/`~W`/`~P` leaf renderer with `~k`/`~K`
- `src/print/pretty.rs` / `src/print/pretty/cind.rs` -> the `io_lib_pretty` port; the `cind/7` family
- `tests/` -> 77 integration files / 789 tests (lib suite 786); the roster pin is `tests/roster_audit.rs`
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
