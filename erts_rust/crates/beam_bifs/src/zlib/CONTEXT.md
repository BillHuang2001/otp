# `beam_bifs::zlib` — the `zlib` module's BIF surface

## Intent
Serve the preloaded `zlib` module inside this crate: the six self-contained convenience functions and the **stream interface** `zlib.erl` is written over.
The six one-shots (`compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`, `gunzip/1`) are registered fast paths that intercept before the Erlang wrappers.
The stream interface is 19 non-exported local functions of `zlib.beam` (`open_nif/0` … `enqueue_nif/2`, `erts/preloaded/src/zlib.erl:100-106`), served here as a NIF-level surface.
The Erlang wrappers stay the argument-validation layer; this node never re-implements `arg_level`/`arg_bitsz`/`arg_mem`/`arg_strategy`/`arg_flush`/`arg_eos_behavior`.
All bytes come from [`beam_deflate`](../beam_deflate) — the sibling crate the workspace keeps as its one zlib 1.3.2 port (`beam_deflate = { path = "../beam_deflate" }` in `Cargo.toml`) — so they are the oracle's bytes rather than merely conforming streams.

## API Surface
- `register(&mut BifRegistry)` — the crate-level entry (`pub(crate)`; `src/registry.rs::with_core_bifs` calls it once, after `time` and before `proc`): the six one-shot keys plus the 19 stream keys, all in module `zlib`, so `zlib` contributes **25** registry keys, every one a stdlib fast path (none is a `bif.tab` row).
- `pub fn serves_local_call(module: &str, function: &str, arity: usize) -> bool` (`mod.rs`, re-exported as `crate::serves_local_call` and in the prelude) — a module-equality short circuit plus a membership test over the 19 keys (the six one-shots are `zlib.erl` functions, not NIF-level calls), mirroring `prim_tty`'s local-call predicate. `beam_rt/src/context/execution.rs::resolve_local_call` is its intended consumer, wired by the **parent node**.
- `NIF_KEYS` (`19` `(name, arity)` rows, `stream::NIF_KEYS`) and `stream::is_nif_key` are the one home of the key table; `register`, `is_nif_key` and `serves_local_call` all read it, so the registry and the predicate cannot disagree.
- No `unsafe` (`#![deny(unsafe_code)]` at the crate root); no crate-local error enum — `beam_common::Result`/`BeamError` and this crate's `Raise`/`ExitReason` machinery only.
- The wire contract of the 19 keys is `erts/emulator/nifs/common/zlib_nif.c`; the design record, the pump algorithm and the oracle goldens are `../../ZLIB_STREAM_BRIEF.md`.

## The 19 keys, one contract each
- `open_nif/0` — mint a handle (an internal `Ref` thing, `is_reference/1` true) for a fresh stream owned by the caller: `ST_NONE`, the `cut` eos behaviour, an empty queue, no stash.
- `close_nif/1` — `ok`; drops the codec, the queue and the stash and latches `Closed` (the registry entry stays as a tombstone, so a second `close` is `not_initialized`).
- `set_controller_nif/2` — `ok`; the caller must be the current controller and the new pid must be **local** (a non-pid or an external pid is `badarg`); no "inside a call" check in this OTP version and **no state precondition** either, so a closed stream is `ok` too (as on the oracle).
- `deflateInit_nif/6` `(Z, Level, Method, WindowBits, MemLevel, Strategy)` — `ok`; the atoms are already integers (the wrappers did that); `method ≠ 8`, an unrepresentable level/`windowBits`/`memLevel` or a strategy past `4` is `deflateInit2`'s `stream_error`, and a second init without an end is `already_initialized`.
- `deflateSetDictionary_nif/2` — the Adler-32 as an integer; a **raw** stream answers `beam_deflate::adler32(0, &bytes)` (zlib does not update `adler` in raw mode), and a **repeated** call succeeds.
- `deflateReset_nif/1` — `ok`; `Deflate::reset` plus `zlib_reset_input` (queue and stash dropped, the `started`/`finished` mirrors cleared), so a finished stream is usable again.
- `deflateEnd_nif/1` — `ok`, or `data_error` when `started && !finished` or the queue is non-empty (the C's `deflateEnd(...) != Z_OK || ioq_size > 0`); the codec is dropped and the state returns to `ST_NONE`.
- `deflateParams_nif/3` — `zlib_return(set_params(level, strategy, &mut [0u8; 1]))`; a **filled** one-byte dummy is `stream_error` (the C's `Z_BLOCK` flush assumed everything was already flushed).
- `deflate_nif/4` `(Z, InChunkSize, OutChunkSize, Flush)` — the pump over `Deflate::deflate`; sets `started` when the call consumed or produced bytes or asked for a flush other than `none`, and `finished` once `eos_seen` latches.
- `inflateInit_nif/3` `(Z, WindowBits, EosBehavior)` — `ok`; records the eos behaviour (only `0..=2`; anything else is `badarg`, decided before the controller check), `is_raw = windowBits < 0`, and clears `eos_seen`/`need_dict_pending`.
- `inflateSetDictionary_nif/2` — `ok`, or `data_error` for a dictionary whose id does not match; on a **fresh framed** stream (no `NeedDict` seen) it is the `stream_error` raise.
- `inflateGetDictionary_nif/1` — a binary of up to 32768 bytes, `<<>>` before any inflate.
- `inflateReset_nif/1` — `ok`; `Inflate::reset` plus `zlib_reset_input`, clearing `eos_seen` and `need_dict_pending`.
- `inflateEnd_nif/1` — `ok`, or `data_error` when `!eos_seen` or the queue is non-empty — the only place a truncated stream is detected.
- `inflate_nif/4` `(Z, InChunkSize, OutChunkSize, Flush)` — the EOS entry step (`error` ⇒ raise `data_error`; `reset` ⇒ `Inflate::reset` and clear `eos_seen`, queue kept; `cut` ⇒ drop the queue) then the pump over `Inflate::inflate`; a `NeedDict` answer latches `need_dict_pending`.
- `getStash_nif/1` — `empty`, or `{ok, Term}` with the ETF image decoded onto the caller's heap; `restore_progress/2` calls it before every `deflate`/`inflate`, so its controller check is the first a **foreign** process hits.
- `setStash_nif/2` — `ok`, or the bare atom `error` raised when a stash already exists.
- `clearStash_nif/1` — `ok`, or the bare atom `error` raised when the stash is empty.
- `enqueue_nif/2` — the only input path: up to `MAX_QUEUE_SEGMENTS` (256, the C's `enif_inspect_iovec(env, 256, …)`) iovec segments are queued and the remainder handed back as `{continue, Tail}`; `[]` is `ok`, a non-iodata element is `badarg`, and the precondition is an open (deflate or inflate) stream.

## Representations
- A handle is an on-heap internal `Ref` thing (`access::make_ref_thing`/`access::ref_thing_id`) whose id starts at `STREAM_ID_BASE` (`1 << 44`), disjoint from the crate's other reference id spaces and inside the 60-bit small-integer slot the id word carries.
- The registry is one process-global `static OnceLock<Mutex<HashMap<u64, Stream>>>`; `lock_streams()` tolerates a poisoned lock (the payload is plain data), and ids are minted from an `AtomicU64`.
- `Stream` holds `state` (`State::{New, Deflate, Inflate, Closed}`, the C's `ST_NONE`/`ST_DEFLATE`/`ST_INFLATE`/closed), `controller: ProcessId`, the codec, `eos_behavior`, `is_raw`, `eos_seen`, `started`/`finished`, `need_dict_pending`, and the input queue and stash below.
- Input is a `VecDeque<Vec<u8>>` with one piece per iovec segment (what `enqueue_nif` pushes and the pump drains).
- The stash is a serialized ETF image (`[131][body]`, `Vec<u8>`), never a raw `Eterm`, so it survives a collection and a heap change; decode goes into a clone of the shared `AtomTable` and commits the names the decoder appended, the way `binconv::decode_term_bif` does it.
- Every entry point runs the same prologue (`with_stream`), in the C's order: decode the handle (`badarg`), look it up (`not_initialized`), check `stream.controller == caller` (`not_on_controlling_process`), then the key's `Requires` state check.
- `Requires::{Init, Deflate, Inflate, Open, Closable}` are the C's `d->state` tests (`Closable` is "anything but `Closed`", `close_nif`'s); `Requires::Any` is the four keys the C gives no state check at all (`set_controller_nif` plus the three stash keys).

## The pump
`pump` loops while `status == Ok`, input is queued, `consumed < input_chunk_size` and the output buffer has room, feeding `min(front.len(), remaining_input)` to the codec and draining that many bytes from the front piece (dropping the piece when it empties); it then does the C's end-of-input flush — one real-flush call with an empty input — when the queue drained and the output buffer was not filled.
Classification order: (1) `DataError`/`StreamError`/`MemError` (`Status::BufError` is **non-fatal** and flows through like `Ok`) → `zlib_return` (`stream_error`/`data_error` raise, `mem_error` a value); (2) `StreamEnd` latches `eos_seen`; (3) zero progress with input still queued → raise `data_error`; (4) output produced ⇒ `[Binary]`, else `[]`; (5) queue drained with room left → `Outcome::Finished`; (6) `NeedDict` → `Outcome::NeedDictionary{adler, out}`; (7) otherwise `Outcome::Continue`.
**Every continuing answer is a tagged tuple** — `outcome_term` builds `{finished, Out}`, `{continue, Out}` and `{need_dictionary, Adler, Out}` (the C's `enif_make_tuple2`/`tuple3`, `zlib_nif.c:459-467`), `Out` being `[Binary]` or `[]`; only `zlib_return`'s `Z_MEM_ERROR` short-circuit answers a bare atom. The tag is load-bearing: `zlib.erl`'s `dequeue_all_chunks_1/3` and `dequeue_next_chunk/2` pattern-match on it, so a bare `Out` list is a `case_clause` in the Erlang layer.
The C's `enif_consume_timeslice` breaks are deliberately not reproduced (a scheduler courtesy, not semantics); `out_pos < output_chunk_size` standing in for the C's `avail_out > 0` assertion is what makes the loop terminate.

## Errors
Errors are the oracle's: `badarg` for arity/type/handle failures, the raised atoms `not_on_controlling_process` / `not_initialized` / `already_initialized` / `stream_error` / `data_error` / the bare `error` for a stash violation, and the plain atoms `mem_error` / `buf_error` / `version_error` as return values.
A tuple reason (`{need_dictionary, Adler}`) rides the `Raise` live-reason term slot.
A capability gap is a typed `BeamError`; never a fabricated value, never `panic!` outside `#[cfg(test)]`.

## Oracle corrections (verified live)
- `inflateSetDictionary` on a fresh **framed** stream is `error:stream_error`, not `data_error`, so `Stream::need_dict_pending` records that the inflater actually reported `Status::NeedDict`; only that flag (or a raw stream) makes the call legal.
- A **repeated** `deflateSetDictionary` succeeds rather than raising, and a **raw** stream's answer is `beam_deflate::adler32(0, &bytes)` rather than the stream's own running Adler.
- `deflateEnd`'s status is inferred from the `started`/`finished` flags on `Stream`, because `beam_deflate` exposes no `status()` the way the C's `z_stream` does.
- After a `finish`, a **repeated** `finish` is *not* an error: zlib stays in `Z_STREAM_END`, so the NIF answers `{finished, []}` (`zlib:deflate/3` ⇒ `[]`). The `stream_error` comes from a flush that is **not** `finish` (`deflate.c:992`, `s->status == FINISH_STATE && flush != Z_FINISH`), with or without queued input; `deflateEnd` then answers `data_error`.
- `inflateEnd_nif` on a fresh, never-fed inflate stream is `error:data_error` (the C requires `eos_seen`, `zlib_nif.c:925`) — a `{deflateEnd,ok}` / `{inflateEnd,data_error}` asymmetry, both oracle-verified.

## Constraints
- Files target ~1000 lines; the surface is split by direction and by responsibility (`one_shot.rs` / `stream/*`), not by count, and `stream/tests.rs` is long only because it embeds the goldens.
- Apache-2.0; the OTP `// %CopyrightBegin% … // %CopyrightEnd%` block on every `*.rs`.
- Never weaken a guard or a golden to make a test pass; a divergence is reported, not smoothed over.

## Test Strategy
In-module `#[cfg(test)]` unit tests over a real `beam_alloc::Heap` + a seeded `beam_term::AtomTable` and a `BifContext`, driving the keys through the crate's own `BifRegistry` the way `zlib.erl`'s wrappers do; assertions on raises go through the two shared test helpers `assert_raise_atom` (class `error`, bare-atom reason, empty stacktrace) and `assert_badarg`, because `BeamError` has no `PartialEq`.
`stream/tests.rs` is the embedded-golden suite (the brief's per-level leaf vectors, the whole 129-byte level-6 stream, the one-shot framings, the dictionary protocol, the `safeInflate` chunk sequence, the `deflateParams` mid-stream total, the prologue/lifecycle guards and the stash round trip) — no test shells out to `erl`, which keeps it CI-safe without an Erlang installation.
The live oracle is `tests/zlib_stream.rs`: the **live-oracle gate for the stream surface**, fail-loud, driving the same sequence and chunk sizes as `zlib.erl`'s wrappers and comparing both directions (the oracle inflates this crate's streams and vice versa); with no usable `erl` it prints a banner and returns, which is **not** a pass.
`tests/zlib_oracle.rs` is that gate for the six one-shots; the counts live in the run, never in this prose.

## Known Issues
- **`serves_local_call` has two in-tree consumers in `beam_rt`:** the local-call seam (`src/context/execution.rs::resolve_local_call`, so `zlib`'s own `-nifs` keys dispatch as BIFs) **and** the `make_fun3` closure indirection (`src/context/apply.rs::served_local_key`) — `zlib.erl` stores the stub fun in a `#zlib_opts{}` record, so `deflate/3`/`inflate/3`/`safeInflate/2` reach their stub through a named local fun, not a local call. `beam_rt`'s four `zlib_*` suites (**29** oracle-cross-checked tests: guards, one-shots, stream-dictionary, stream-levels) cover the stream route end to end.
- No `tools/differential` case and no `tools/bench` entry covers the 19 stream keys (the corpus is the sibling `../../tools/differential/`); the six one-shots keep their oracle suite.
- `close_nif` sets `State::Closed` but never removes the registry entry, so closed streams are tombstones (bounded, never reclaimed); there is likewise no process-exit observer, so a dead controller's streams stay in the registry unreachable.
- `Z_ERRNO`/`Z_VERSION_ERROR` are unmodelled — `beam_deflate` never reports them, so the C's errno/version arms are absent; a `Status::NeedDict` reaching `zlib_return` lands on the C's `default:` arm (`unknown_error`), which the pump's own `NeedDict` classification makes unreachable.
- **Four keys carry no state precondition** (`Requires::Any`): `set_controller_nif` and the three stash keys, as in the C (`zlib_nif.c:473-522`, `:583-602`). Everything else raises `not_initialized`/`already_initialized` exactly where the C's `d->state` test does.

## Routing Table
- `mod.rs` → the whole-surface module doc, submodule wiring, `register()`, `serves_local_call`, the module suite
- `one_shot.rs` → the six convenience functions and their in-module suite
- `stream/mod.rs` → `State`/`Stream`/`Requires`, the registry, handle minting, the `with_stream` prologue, the argument tables (`int_arg`/`chunk_arg`/`flush_arg`/`strategy_arg`), `NIF_KEYS`/`is_nif_key`, `register()`
- `stream/pump.rs` → `zlib_return`, the shared pump over a codec closure, `Outcome`/`Progress`, `outcome_term`
- `stream/lifecycle.rs` → `open`/`close`/`set_controller`/`enqueue` and the three stash keys
- `stream/deflate.rs` → the six `deflate*_nif` keys
- `stream/inflate.rs` → the six `inflate*_nif` keys
- `stream/tests.rs` → the embedded-golden suite (`#[cfg(test)]`; long by design, the same convention as `beam_deflate/tests/deflate_goldens.rs`)
- `../beam_deflate/` → the zlib 1.3.2 port every byte comes from (`CONTEXT.md` § *Consumer shapes*; `tests/roundtrip.rs` re-implements the `zlib_nif.c` pump as executable reference)
- `../../ZLIB_STREAM_BRIEF.md` → the work brief / design record: the C NIF surface, the pump algorithm, the representation and the oracle goldens
