# `beam_bifs::zlib` — the `zlib` module's BIF surface

## Intent
Serve the preloaded `zlib` module inside this crate: the six self-contained convenience functions and the **stream interface** `zlib.erl` is written over.
The six one-shots (`compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`, `gunzip/1`) are registered fast paths that intercept before the Erlang wrappers.
The stream interface is 19 non-exported local functions of `zlib.beam` (`open_nif/0` … `enqueue_nif/2`, `erts/preloaded/src/zlib.erl:100-106`) that a runtime has to serve as a NIF surface.
The Erlang wrappers stay the argument-validation layer; this node never re-implements `arg_level`/`arg_bitsz`/`arg_mem`/`arg_strategy`/`arg_flush`/`arg_eos_behavior`.
All bytes come from [`beam_deflate`], the workspace's one zlib 1.3.2 port, so they are the oracle's bytes rather than merely conforming streams.

## API Surface
- `register(&mut BifRegistry)` — the crate-level entry (`lib.rs` calls it once, last after `ets`): the six one-shot keys plus the 19 stream keys, all in module `zlib`, so `zlib` contributes 25 registry keys.
- `pub fn serves_local_call(module: &str, function: &str, arity: usize) -> bool` — the local-call predicate `beam_rt/src/context/execution.rs::resolve_local_call` consults (module-equality short circuit + a membership test over the 19 keys), mirroring `prim_tty::serves_local_call`.
- `NIF_KEYS` (`19` `(name, arity)` rows, exposed crate-internally as `stream::NIF_KEYS`) and `stream::is_nif_key` are the one home of the key table; `register`, `is_nif_key` and `serves_local_call` all read it.
- No `unsafe` (`#![deny(unsafe_code)]` at the crate root); no crate-local error enum — `beam_common::Result`/`BeamError` and this crate's `Raise`/`ExitReason` machinery only.
- The wire contract of the 19 keys is `erts/emulator/nifs/common/zlib_nif.c`; the design record, the pump algorithm and the oracle goldens are `../../ZLIB_STREAM_BRIEF.md`.

## Representations
- A handle is an on-heap internal `Ref` thing whose id starts at `STREAM_ID_BASE` (`1 << 44`), disjoint from the crate's other reference spaces and inside the 60-bit small-integer slot.
- The registry is one process-global `OnceLock<Mutex<HashMap<u64, Stream>>>`; `lock_streams()` tolerates a poisoned lock (the payload is plain data), and ids are minted from an `AtomicU64`.
- `Stream` holds `state` (`State::{New, Deflate, Inflate, Closed}`, the C's `ST_NONE`/`ST_DEFLATE`/`ST_INFLATE`/closed), `controller: ProcessId`, the codec, `is_raw`, `eos_seen`, `started`/`finished`, `need_dict_pending`, and the two queues below.
- Input is a `VecDeque<Vec<u8>>` with one piece per iovec segment (what `enqueue_nif` pushes and the pump drains), bounded at `MAX_QUEUE_SEGMENTS = 256`.
- The stash is a serialized ETF image (`[131][body]`, `Vec<u8>`), never a raw `Eterm`, so it survives a collection and a heap change.
- Every entry point runs the same prologue: decode the handle (`badarg`), look it up (`not_initialized`), check `stream.controller == caller` (`not_on_controlling_process`), then the key's `Requires` state check.
- `Requires::{Init, Deflate, Inflate, Open}` are the state preconditions; `Requires::Closable` is the catch-all "anything but `Closed`" for the keys the C gives no state check.

## File ownership
- `mod.rs` → the whole-surface module doc, submodule wiring, `State`/`Stream`/`Requires`, the registry and handle minting, the `with_stream` prologue, the argument tables (`arg_level`/`arg_bitsz`/`arg_mem`/`arg_strategy`/`arg_flush` tables), `NIF_KEYS`/`is_nif_key`, `register()`, `serves_local_call`.
- `one_shot.rs` → the six convenience functions and their in-module suite (unchanged by the stream work).
- `stream/pump.rs` → `zlib_return`, the shared `pump` over a `codec` closure, `Outcome`/`Progress`, `outcome_term`.
- `stream/lifecycle.rs` → `open_nif`, `close_nif`, `set_controller_nif`, `enqueue_nif` and the three stash keys (`getStash_nif`/`setStash_nif`/`clearStash_nif`).
- `stream/deflate.rs` → `deflateInit_nif`, `deflateSetDictionary_nif`, `deflateReset_nif`, `deflateEnd_nif`, `deflateParams_nif`, `deflate_nif`.
- `stream/inflate.rs` → `inflateInit_nif`, `inflateSetDictionary_nif`, `inflateGetDictionary_nif`, `inflateReset_nif`, `inflateEnd_nif`, `inflate_nif`.

## The pump's classification order
`pump` loops while `status == Ok` with input left, queue non-empty, `consumed < input_chunk_size` and room in the output buffer, feeding `min(front.len(), remaining_input)` to the codec and draining that many bytes from the front piece (dropping the piece when it empties); it then does the C's end-of-input flush when the queue drained with room left.
Classification: (1) `DataError`/`StreamError`/`MemError` → `zlib_return` (`mem_error` as a value, `stream_error`/`data_error` as raises); (2) `StreamEnd` latches `eos_seen`; (3) zero progress with input still queued → raise `data_error`; (4) queue drained with room left → `Outcome::Finished`; (5) `NeedDict` → `Outcome::NeedDictionary{adler, out}` → the `{need_dictionary, Adler, Out}` tuple; (6) otherwise `Outcome::Continue`.
The C's `enif_consume_timeslice` breaks are deliberately not reproduced (a scheduler courtesy, not semantics); `out_pos < output_chunk_size` standing in for the C's `avail_out > 0` assertion is what makes the loop terminate.

## Errors
Errors are the oracle's: `badarg` for arity/type/handle failures, the raised atoms `not_on_controlling_process` / `not_initialized` / `already_initialized` / `stream_error` / `data_error` / the bare `error` for a stash violation, and the plain atoms `mem_error` / `buf_error` / `version_error` as return values.
A tuple reason (`{need_dictionary, Adler}`) rides the `Raise` live-reason term slot.
A capability gap is a typed `BeamError`; never a fabricated value, never `panic!` outside `#[cfg(test)]`.

## Oracle corrections (verified live)
- `inflateSetDictionary` on a fresh **framed** stream is `error:stream_error`, not `data_error`, so `Stream::need_dict_pending` records that the inflater actually reported `Status::NeedDict`; only that flag (or a raw stream) makes the call legal.
- A **repeated** `deflateSetDictionary` succeeds rather than raising, and a **raw** stream's answer is `beam_deflate::adler32(0, &bytes)` rather than the stream's own running Adler.
- `deflateEnd`'s status is inferred from the `started`/`finished` flags on `Stream`, because `beam_deflate` exposes no `status()` the way the C's `z_stream` does.

## Constraints
- Files target ~1000 lines; the stream surface is split by direction and by responsibility (`one_shot.rs` / `stream/*`), not by count.
- Apache-2.0; the OTP `// %CopyrightBegin% … // %CopyrightEnd%` block on every `*.rs`.
- Never weaken a guard or a golden to make a test pass; a divergence is reported, not smoothed over.

## Test Strategy
In-module `#[cfg(test)]` unit tests over a real `beam_alloc::Heap` + a seeded `beam_term::AtomTable` and a `BifContext`; assertions on raises go through the two shared test helpers `assert_raise_atom` (class `error`, bare-atom reason, empty stacktrace) and `assert_badarg`, because `BeamError` has no `PartialEq`.
The oracle-cross-checked goldens at the crate level are `tests/zlib*.rs` (the six one-shots' byte identity against a live `erl`, skipped loudly when it is absent) and `tests/zlib_oracle.rs`; no `tests/` file covers the 19 stream keys — their evidence is the in-module suites plus `ZLIB_STREAM_BRIEF.md`'s recorded goldens.

## Known Issues
- `serves_local_call`/`NIF_KEYS`/`is_nif_key` have no in-tree consumer yet (`#[allow(dead_code)]` with a rationale): `beam_rt`'s `resolve_local_call` must consult `serves_local_call` before a `zlib` local call is entered as code — a host wiring task, since this crate may not edit `beam_rt`.
- No `tools/differential` case and no `tools/bench` entry exists for the 19 stream keys; the six one-shots keep their oracle suite.
- `close_nif` sets `State::Closed` but never removes the registry entry, so closed streams are tombstones (bounded, never reclaimed); there is likewise no process-exit observer, so a dead controller's streams stay in the registry unreachable.
- `Z_ERRNO`/`Z_VERSION_ERROR` are unmodelled — `beam_deflate` never reports them, so the C's errno/version arms are absent; a `Status::NeedDict` reaching `zlib_return` lands on the C's `default:` arm (`unknown_error`), which the pump's own `NeedDict` classification makes unreachable.
- `Requires::Closable` stands in for the C's "no state precondition" keys, so an ill-timed call answers `not_initialized` rather than whatever the C's `ASSERT`-guarded path would have done.

## Routing Table
- `mod.rs` → the whole-surface module doc, submodule wiring, `register()`, `serves_local_call`, the key tables
- `one_shot.rs` → the six convenience functions and their in-module suite
- `stream/mod.rs` → state, registry, handle minting, the prologue, the argument tables, `NIF_KEYS`
- `stream/pump.rs` → `zlib_return`, the shared pump, `Outcome`/`Progress`
- `stream/lifecycle.rs` → `open`/`close`/`set_controller`/`enqueue` and the three stash keys
- `stream/deflate.rs` → the six `deflate*_nif` keys
- `stream/inflate.rs` → the six `inflate*_nif` keys
