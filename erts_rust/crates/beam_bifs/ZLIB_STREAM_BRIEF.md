# `zlib` stream API at the NIF level — work brief

This is the authoritative brief for the `zlib` stream-API work in `beam_bifs`.
It is a *work* document (what to build and why), not crate documentation; the
crate's `CONTEXT.md` files carry the current state.

## Two facts that define the design (already verified end-to-end — do not re-investigate)

1. The workspace has `erts_rust/crates/beam_deflate` — the complete, oracle-byte-exact
   zlib 1.3.2 port (deflate **and** inflate). Safe Rust, `#![forbid(unsafe_code)]`,
   140 tests green, 742-input corpus byte-identical, no dependency beyond `beam_common`.
   Read its `CONTEXT.md` (especially *Consumer shapes*) and
   `../beam_deflate/tests/roundtrip.rs` — the latter re-implements the `zlib_nif.c`
   pump loop as executable reference. `beam_bifs` takes it as a path dependency
   (`beam_deflate = { path = "../beam_deflate" }`); the crate is then an automatic
   workspace member (a path dependency of a member), so the root `Cargo.toml`
   `members` list does not have to change.
2. **`zlib.erl` itself must keep doing the work.** `erts/preloaded/src/zlib.erl` is a
   real preloaded module and its wrapper bodies already run correctly on our engine
   (records, a `fun` stored in a record, `try/after`, `bif raise`,
   `erlang:iolist_to_iovec/1`, `{continue,_}`/`{finished,_}` recursion — all proven).
   What is missing is the 19 NIFs, which are **non-exported local functions** of
   `zlib.beam` and therefore unreachable from the wrapper bodies today (each call dies
   as `undef` at its stub). **Implement the NIF-level surface and keep the Erlang
   wrappers** — do not re-implement the wrappers' argument validation in Rust
   (`arg_level`/`arg_bitsz`/`arg_mem`/`arg_strategy`/`arg_flush`/`arg_eos_behavior`
   already produce the documented error shapes), or those shapes fork.
   The six one-shot names `compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`,
   `gunzip/1` are already served by this crate's high-level fast paths, which
   intercept before the wrappers run — **keep them working and byte-identical**.

## Ground truth (read-only, never modify)

- `erts/emulator/nifs/common/zlib_nif.c` (996 lines) — the C NIF layer. Every
  observable is here: `zlib_return`, `zlib_process_check`, `zlib_reset_input`,
  `zlib_flush_queue`, `zlib_codec`, and the 19 NIF bodies.
- `erts/preloaded/src/zlib.erl` (929 lines) — the Erlang layer: the `-nifs([...])`
  list, `-removed([...])`, the chunk-size macros, `deflate/3`, `inflate/2,3`,
  `safeInflate/2`, `deflateParams/3`, `restore_progress/2`, `save_progress/3`,
  `enqueue_input/2`.
- `erts/preloaded/src/zlib.erl:100-106` — the NIF name/arity list.

## The NIF surface to serve (19 keys, module `zlib`)

`open_nif/0`, `close_nif/1`, `set_controller_nif/2`, `deflateInit_nif/6`,
`deflateSetDictionary_nif/2`, `deflateReset_nif/1`, `deflateEnd_nif/1`,
`deflateParams_nif/3`, `deflate_nif/4`, `inflateInit_nif/3`,
`inflateSetDictionary_nif/2`, `inflateGetDictionary_nif/1`, `inflateReset_nif/1`,
`inflateEnd_nif/1`, `inflate_nif/4`, `getStash_nif/1`, `clearStash_nif/1`,
`setStash_nif/2`, `enqueue_nif/2`.

Universal prologue, in this order **[C]**: (1) arity/type validation failures ⇒
`badarg`; (2) caller is not the stream's controlling process ⇒ raise
`not_on_controlling_process`; (3) state precondition ⇒ raise `not_initialized` or
`already_initialized`.

Return/exception translator (`zlib_return`): `Z_OK`→`ok`, `Z_STREAM_END`→`stream_end`,
`Z_STREAM_ERROR`→**raise** `stream_error`, `Z_DATA_ERROR`→**raise** `data_error`,
`Z_MEM_ERROR`→atom `mem_error`, `Z_BUF_ERROR`→atom `buf_error`,
`Z_VERSION_ERROR`→atom `version_error`, `Z_ERRNO`→an integer (`errno`; unreachable in
this port — no syscall layer — but keep the arm as a typed gap). Only
`stream_error`/`data_error` are exceptions; the other atoms are plain return values.

### Per-NIF essentials

- `open_nif/0` → a **reference-like** term (`is_reference` must be `true`) naming a
  `ST_NONE` stream owned by the caller.
- `close_nif/1` → `ok`; runs `deflateEnd`/`inflateEnd` when in a stream state, drops
  the stash and the input queue, → `ST_CLOSED`. A second `close` ⇒ `not_initialized`.
- `set_controller_nif/2` → `ok`; caller must be the current owner; the new pid must be
  **local** (a remote pid ⇒ `badarg`; `ProcessId::from_eterm` already rejects a
  non-pid and an external pid). No "inside a call" check in this OTP version.
- `deflateInit_nif/6` `(Z, Level, Method, WindowBits, MemLevel, Strategy)` and
  `inflateInit_nif/3` `(Z, WindowBits, EosBehavior)`: the atoms have already been
  translated to integers by `zlib.erl`. Success → `ok` (also on `stream_end`/atom
  returns); a rejected configuration surfaces as the translator says (e.g. windowBits
  `-8` ⇒ raise `stream_error`). Second init without an end ⇒ raise
  `already_initialized`. `inflateInit` also records the eos behaviour and
  `is_raw = windowBits < 0`.
- `deflate_nif/4` / `inflate_nif/4` `(Z, InChunkSize, OutChunkSize, Flush)` — **this is
  the pump** (see below). Chunk sizes are `?DEFLATE_IN_CHUNKSIZE 8192` /
  `?DEFLATE_OUT_CHUNKSIZE 8192` for deflate and `?INFLATE_IN_CHUNKSIZE 8192` /
  `?INFLATE_OUT_CHUNKSIZE 16384` for inflate.
- `enqueue_nif/2` — the only input path (`enif_inspect_iovec` with a 256-segment cap; a
  non-iodata ⇒ `badarg`; `ok` or `{continue, Tail}` when a tail remains; a state
  outside DEFLATE/INFLATE ⇒ `not_initialized`). `zlib.erl`'s `enqueue_input/2` loops on
  `{continue, Tail}`.
- The **stash**: `getStash_nif/1` → `empty` | `{ok, Term}`; `setStash_nif/2` → `ok` but
  raises the bare atom `error` if a stash already exists; `clearStash_nif/1` → `ok` but
  raises the bare atom `error` if empty. `deflateReset`/`inflateReset`/`deflateEnd`/
  `inflateEnd`/`close` also drop it. `zlib.erl` uses it for `deflateParams/3` and for
  `inflate/3`'s `exception_on_need_dict` progress.
- `deflateEnd_nif/1` → `ok`, but raise `data_error` if `deflateEnd` would not return
  `Z_OK` (i.e. the last `deflate` was not `finish`) or the input queue is non-empty;
  state ≠ DEFLATE ⇒ `not_initialized`. `inflateEnd_nif/1` likewise raises `data_error`
  when `!eos_seen` or the queue is non-empty — **the only place a truncated stream is
  detected**. Both reset the queue + stash and set the state to `ST_NONE`.
- `deflateParams_nif/3` `(Z, Level, Strategy)` → `ok` (delegate to `Deflate::set_params`
  with a 1-byte dummy output buffer; a filled buffer ⇒ `stream_error`).
- `deflateSetDictionary_nif/2` → the Adler32 as an integer (`Deflate::set_dictionary`;
  a raw stream's answer is the adler of the dictionary bytes themselves, because zlib
  does not update `adler` in raw mode); `inflateSetDictionary_nif/2` → `ok` (a bad
  dictionary is zlib's `Z_DATA_ERROR` ⇒ raise `data_error`); `inflateGetDictionary_nif/1`
  → a binary (up to 32768 B, `<<>>` before any inflate).
- `deflateReset_nif/1` / `inflateReset_nif/1` → `ok`, clearing the queue and the stash.
  `deflateReset` after `finish` must work (the stream becomes usable again).
- `inflate_nif/4`'s entry has one extra step **[C]** before the pump: when `eos_seen`
  and the queue is non-empty, `error` ⇒ raise `data_error`, `reset` ⇒
  `Inflate::reset()` and clear `eos_seen`, `cut` ⇒ drop the queue.

### The pump (`zlib_nif.c::zlib_codec` + `zlib_flush_queue`)

```
out = an output_chunk_size buffer
for each queued input piece, while status == Ok and consumed < InChunkSize:
    pump(input_piece_slice, out, Flush::None); accumulate consumed
drain the consumed prefix from the queue
if status == Ok and the queue is empty and produced < output_chunk_size:
    pump(&[], out, <the caller's real Flush>)
classify: Z_STREAM_END -> eos_seen = 1;
          consumed == 0 && produced == 0 && queue non-empty -> raise data_error
          produced > 0 -> Out = [shrink(out, produced)] else Out = []
          queue empty && produced < output_chunk_size -> {finished, Out}
          else if status != NeedDict            -> {continue,  Out}
          else                                  -> {need_dictionary, Adler, Out}
```

Notes:
- `status` before classification: the C returns `Z_BUF_ERROR` as a *non-fatal* code
  (`res < 0 && res != Z_BUF_ERROR` ⇒ error return), so `BufError` flows into the
  classification like `Ok` does (it is `!= Z_NEED_DICT`, so the answer is `{continue,…}`
  or `{finished,…}` per the queue/short-write test).
- `{finished, _}` does **not** mean `Z_STREAM_END` — it means "queue drained and the
  output buffer was not filled".
- Do not reproduce `enif_consume_timeslice`'s mid-block breaks (scheduler-dependent
  noise); the observable contract to keep is that the *concatenation* is byte-exact and
  every leaf is `<=` the chunk size.
- `Adler` in `{need_dictionary, Adler, Out}` is `Status::NeedDict(u32)`.

## Representation (this crate's established stateful-BIF shape)

- **Handle**: an on-heap internal `Ref` thing — `access::make_ref_thing(ctx.heap(), id)`
  / `access::ref_thing_id(term)` — exactly as `src/binary.rs::compiled_patterns`,
  `src/ets/specs.rs` and `src/runtime/atomics.rs` do it. `is_reference/1` is `true`
  for that term, and refs survive map keys, ETS keys and an ETF round trip, so the
  handle behaves like the C's resource term for everything `zlib.erl` does with it.
  Take an id-space base disjoint from the existing ones (compiled patterns `1 << 32`,
  ETS continuations `1 << 40`, reverse continuations `1 << 41`, compiled specs
  `1 << 48`, runtime resources `1 << 52`) — `1 << 44` is free — and mint ids from a
  module-local counter.
- **Stream table**: `static OnceLock<Mutex<HashMap<u64, Stream>>>`, the same pattern as
  those registries (poison-tolerant `lock().unwrap_or_else(PoisonError::into_inner)`).
  A `Stream` carries: `state` (`None`/`Deflate`/`Inflate`/`Closed`), `controller:
  ProcessId`, `queue: VecDeque<Vec<u8>>` (or a `Vec` + head offset; `enif_ioq` is a
  FIFO of byte pieces, so a `VecDeque<Vec<u8>>` with a consumed-prefix drain is the
  natural model), the `Option<beam_deflate::Deflate>` / `Option<beam_deflate::Inflate>`
  instance, `eos_behavior: u8`, `eos_seen: bool`, `is_raw: bool`, `stash:
  Option<Stash>`. Drop the entry on `close` (a `close` must not leak; the C relies on
  GC, we cannot).
- **Stash**: the term must be copied out of the caller's heap into storage this crate
  owns (the caller's heap is rewound after the BIF) and copied back on retrieval.
  The safe route is an **ETF image**: encode with `crate::binconv`'s internal
  `encode_body`/`plain_image_of` and decode with `crate::binconv`'s
  `decode_term_with_options_used` onto `ctx.heap()` on retrieval (both are
  `pub(crate)`; atoms land in the runtime's one shared table). The stash term is
  `{Kind, iolist}` in practice (refc binaries / nothing), so the round trip is cheap
  and cannot produce a stale word. Do **not** store a raw `Eterm` in the registry —
  that is the crate's documented stale-word hazard (`src/ets/CONTEXT.md`,
  `TableRegistry::insert_spec`).

## Where the code goes (files under ~1000 lines)

`src/zlib.rs` is 1220 lines today and grows a lot, so it becomes a directory:

- `src/zlib/mod.rs` — module doc (the whole `zlib` surface: the six one-shots **and**
  the stream API), submodule wiring, `register()`, `serves_local_call()`.
- `src/zlib/one_shot.rs` — the six convenience functions, re-pointed at
  `beam_deflate` (no behaviour change; `tests/zlib.rs` and `tests/zlib_oracle.rs`
  are the pins).
- `src/zlib/stream.rs` — the `Stream` state, the registries, the pump, the stash, the
  controller checks, and the 19 `BifFn` bodies (split further — e.g. `stream/deflate.rs`
  / `stream/inflate.rs` — if it wants to pass ~1000 lines).
- `src/zlib/nif.rs` — (optional) the thin per-NIF argument decoding + prologue if that
  keeps `stream.rs` readable.

Deleted by this work: `src/zlib_deflate/` (2030 lines — now `beam_deflate`) and
`src/deflate.rs` (729 lines, the crate-local RFC 1951 decoder — `beam_deflate`'s
`Inflate` supersedes it; verify nothing else in the crate uses it first).

## Helper surface this crate already has (use it, do not reinvent)

- Terms: `beam_term` accessors only, via `crate::access` (`make_ref_thing`,
  `ref_thing_id`, `make_heap_bitstring`, `is_atom`, `tuple_element`, `int_value`, …).
- Iodata / iolists: `crate::binary::iodata_bytes(root)` (the `iolist_to_iovec`
  accepted set) for a flat `Vec<u8>`; for `enqueue_nif` the argument is already an
  `erlang:iolist_to_iovec/1` output (a list of binaries, possibly with an improper
  tail), so walk it up to the 256-segment cap and report the remaining list as the
  `{continue, Tail}` tail.
- Errors/raises: `crate::error::{raise_error, raise_exit, raise_term, raise_error_term,
  raise_error}` and `beam_common::{Raise, ExitReason, ExceptionClass}`. The atom-only
  reasons are `not_on_controlling_process`, `not_initialized`, `already_initialized`,
  `stream_error`, `data_error`, and the bare atom `error` for a stash violation — all
  plain `error:<atom>` raises. A tuple reason (`{need_dictionary, Adler}`) goes through
  the live-reason term slot (`raise_term`/`with_live_reason`) so `beam_rt` renders it
  faithfully.
- State: `crate::context::BifContext` (`heap()`, `atoms()`, `self_pid()`, `atom(name)`,
  `int`, `tuple`, `cons`, `list`, `make_bin`-equivalents).
- Checksums: `beam_deflate::{adler32, crc32}` — never a local copy.

## Values that must hold (oracle OTP 29.1.1, live-verified)

- `deflate` with `none` then `deflateEnd` ⇒ `data_error`; `deflateEnd` with no deflate
  call at all ⇒ `ok`; `deflate` twice with `finish` ⇒ the second raises `stream_error`;
  `deflate` on an inflate stream ⇒ `not_initialized`; `deflateInit(make_ref(),
  default)` ⇒ `badarg`; a foreign process calling `zlib:deflate/3` dies with
  `error:not_on_controlling_process` — and the **first** NIF it reaches is
  `getStash_nif/1` (via `restore_progress/2`), so that check must be in place there.
- `deflateParams` mid-stream: level 0, `deflate(Z,<<"A"*16>>,none)` → `[2]`,
  `deflateParams(Z,9,default)` → `ok`, next `deflate(Z,<<"BBBB">>,finish)` → two
  binaries `[26, 8]`, total 36 B, and `zlib:uncompress` round-trips to
  `<<"AAAAAAAAAAAAAAAABBBB">>`.
- Pump goldens (byte-exact; use them as tests). Fixture `F1` = 1024 B:
  `binary:part(binary:copy(<<"It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of foolishness, it was the epoch of belief, it was the epoch of incredulity.\n">>, 6), 0, 1024)`,
  md5 `A1D3CA40959CD47D539D2A0115E99AF2`. `deflateInit(Z,Level)` then 4 ×
  `deflate(Z, 256 B, sync)` then `deflate(Z, [], finish)`, per level — leaf-size vector
  / total / md5 of the concatenation:
  level 0 `[[268],[266],[266],[266],[9]]` 1075 `744E6AB1F1679A65419D31433B4CE009`;
  1 `[[102],[11],[9],[9],[6]]` 137 `1483D581C7312DE6CF3B5536AC760D8D`;
  2 and 3 `[[100],[11],[9],[9],[6]]` 135 `CA8610AA9D9C8629E0A160295B113D3D`;
  4 and 5 `[[96],[9],[9],[9],[6]]` 129 `A33B7229B7EDF94FDA7E879668E71BDE`;
  6 `[[96],[9],[9],[9],[6]]` 129 `559A0ED1917748B7D484AE0BF201F1DD`;
  7, 8 and 9 `[[96],[9],[9],[9],[6]]` 129 `FA42B81D126127EA40E9AA4A1F456C6B`.
  Level 6's full 129-byte stream:
  `789CB4CEC10980301044D1BB556C01621F29439389598859C9AE04BB979C44D0A3E7F707C619B559C91268`
  `811A4924E30D3A12DFD2A47ED1BCA243630DB2BD4914C9ACA9409F43ECE2530F1664467C372EBE221C99ED`
  `9C06F7C7D50B0000FFFF1AE94E05000000FFFF1AE94E05000000FFFF1AE94E05000000FFFF0300499E6670`.
- One-shot `deflate(Z,F1,finish)` at level 6/wbits 15/mem 8/default = 102 B, md5
  `6518CCA11F87CB76E961FB89CC729D7A`:
  `789CEDCEC10980301044D1BB556C01621F96A171621692ACB82BC1EEC59308F1E631E737037F342A9392`
  `05D00C35124FC609DA133F5264FFA269C50D85759154132F12594386BE8FD8C4857B3023327CDD38BB1D`
  `CB11D9CEA11B5B6A4B6DA9FFA65E499E6670`; wbits `-15` = those middle 96 B (md5
  `D4C219FCE1CCF7630F2F3C8468048FD2`); wbits `31` = `1F8B0800000000000003` + the
  96 B + `4FDC6D0200040000`; strategy `filtered` = 104 B md5
  `A3DBFF853CCF2E0F9A23AB31DF3055AC`; `huffman_only` and `rle` = the same 546 B (md5
  `2FD995AF6E54E35AABA5B103CCBB4059`).
- Small: `zlib:zip(<<"hello hello hello">>)` = `CB48CDC9C957C8409000`,
  `zlib:compress(same)` = `789CCB48CDC9C957C84090003A2E067D`,
  `zlib:gzip(<<"hello">>)` = `1F8B0800000000000003CB48CDC9C9070086A6103605000000`.
- `safeInflate/2`: `{continue, iolist()} | {finished, iolist()} |
  {need_dictionary, Adler, iolist()}` — a 100000-byte payload compressed to 494 B
  yields `{continue,[16384]} ×6` then `{finished,[1696]}`, and a further
  `safeInflate(Z,[])` → `{finished,[]}`.
- `need_dictionary`: dictionary
  `<<"a dictionary with repeated words words words and more words">>`, payload
  `<<"words words words and more words words">>`; `deflateSetDictionary` →
  `2662864480`, `deflate(finish)` → `78BB9EB8166023240F21011D420E92`; then
  `inflate/2` raises `error:{need_dictionary,2662864480}`, `inflate/3` with
  `[{exception_on_need_dict,false}]` returns `{need_dictionary,2662864480,[]}`,
  `safeInflate/2` returns the same tuple, `inflateSetDictionary` → `ok`,
  `inflate(I,[])` → `[<<"words words words and more words words">>]`.

## Do NOT invent the removed functions

`zlib:setBufSize/2`, `getBufSize/1`, `crc32/1,2,3`, `adler32/2,3`, `crc32_combine/4`,
`adler32_combine/4`, `inflateChunk/1,2` are in `zlib.erl`'s `-removed` list and yield
`error:undef` on the oracle — that is the correct behaviour to preserve. Their
replacements `erlang:crc32/1,2`, `erlang:adler32/1,2`, `erlang:crc32_combine/3`,
`erlang:adler32_combine/3` are the real BIFs (note: `erlang:adler_combine/3` does not
exist — the OTP doc string is a typo). Check whether this crate already serves them and
only fill a real gap.

## Local-call predicate the runtime will consume

`beam_rt` dispatches a *local* call to a host-served function only through
`beam_rt/src/context/execution.rs::resolve_local_call`, which today short-circuits on
`prim_tty` alone. `beam_bifs` must expose the equivalent predicate for `zlib`:

```rust
pub fn serves_local_call(module: &str, function: &str, arity: usize) -> bool
```

— a module-equality short circuit plus a membership test over the 19 keys — mirroring
the existing `prim_tty` shape, re-exported from `lib.rs`. `beam_rt` is wired by the
parent in a later round; **do not modify `beam_rt`**.

## Constraints

- Write only under `erts_rust/crates/beam_bifs/`. `beam_rt`, `beam_codec` and
  `beam_deflate` are other nodes.
- `beam_common::Result`/`BeamError` for every fallible path; the crate's existing
  `Raise`/`ExitReason` machinery for raises. No `unsafe`, no new external
  dependencies, Apache-2.0 OTP header block on every `*.rs`, files under ~1000 lines.
- `#![deny(unsafe_code)]` at the crate root stays.
- Oracle-cross-checked tests must **skip loudly** when `erl` is absent; serialize `erl`
  boots (`beam_deflate/tests/zsupport` has the harness; `tests/zlib_oracle.rs` here
  already has the pattern) if tests are parallelized.
- **Byte identity or a typed error — never "close enough". Never weaken a golden or a
  guard to make a test pass.**
- Build/test from the repository root, never `cd`:
  `erts_rust/scripts/cargo.sh build|test --manifest-path erts_rust/Cargo.toml -p beam_bifs --all-targets`
  (sandbox `$HOME` is read-only). **Always pass an explicit long timeout (900000 ms)** —
  a call cut off at ~10 s leaves truncated object files. Run the crate's full suite with
  `--no-fail-fast` and report the counts.
