# `beam_bifs::zlib` — the `zlib` module's BIF surface

## Intent
Serve the preloaded `zlib` module inside this crate: the six self-contained convenience
functions (`compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`, `gunzip/1`) as
registered fast paths, and the **stream interface** `zlib.erl` is written over — 19
non-exported local functions of `zlib.beam` (`open_nif/0` … `enqueue_nif/2`,
`erts/preloaded/src/zlib.erl:100-106`) that a runtime has to serve as a NIF surface.
The Erlang wrappers stay the argument-validation layer; this node never re-implements
`arg_level`/`arg_bitsz`/`arg_mem`/`arg_strategy`/`arg_flush`/`arg_eos_behavior`.
All bytes come from [`beam_deflate`], the workspace's one zlib 1.3.2 port, so they are the
oracle's bytes rather than merely conforming streams.

## API Surface
- `register(&mut BifRegistry)` — the crate-level entry (`lib.rs` calls it once, last after
  `ets`): the six convenience keys plus the 19 stream keys, all in module `zlib`.
- `pub fn serves_local_call(module: &str, function: &str, arity: usize) -> bool` — the
  local-call predicate `beam_rt/src/context/execution.rs::resolve_local_call` consults
  (module-equality short circuit + a membership test over the 19 keys), mirroring the
  existing `prim_tty` shape. Re-exported from `lib.rs`. `beam_rt` is wired by the parent
  node, not here.
- No `unsafe` (`#![deny(unsafe_code)]` at the crate root); no crate-local error enum —
  `beam_common::Result`/`BeamError` and this crate's `Raise`/`ExitReason` machinery only.
- The wire contract of the 19 keys is `erts/emulator/nifs/common/zlib_nif.c` (the C NIF
  layer) and `erts/preloaded/src/zlib.erl` (the caller); the design record, the pump
  algorithm and the oracle goldens are `../../ZLIB_STREAM_BRIEF.md`.

## Constraints
- Errors are the oracle's: `badarg` for arity/type failures, the raised atoms
  `not_on_controlling_process` / `not_initialized` / `already_initialized` /
  `stream_error` / `data_error` / the bare `error` for a stash violation, and the plain
  atoms `mem_error` / `buf_error` / `version_error` as return values. A tuple reason
  (`{need_dictionary, Adler}`) rides the `Raise` live-reason term slot.
- A capability gap is a typed `BeamError`; never a fabricated value, never `panic!`.
- Files target ~1000 lines; the stream surface is split by direction and by
  responsibility (`one_shot.rs` / `stream*` / `nif`), not by count.
- Apache-2.0; the OTP `// %CopyrightBegin% … // %CopyrightEnd%` block on every `*.rs`.

## Test Strategy
In-module `#[cfg(test)]` unit tests over a real `beam_alloc::Heap` + a seeded
`beam_term::AtomTable` and a `BifContext`; the oracle-cross-checked goldens (pump leaf
vectors, per-level stream md5s, `safeInflate` chunk sequences, the dictionary protocol)
are in `tests/zlib*.rs` at the crate level. `erl`-dependent tests skip loudly when the
system `erl` is absent and serialize boots.

## Routing Table
- `mod.rs` → the whole-surface module doc, submodule wiring, `register()`,
  `serves_local_call`
- `one_shot.rs` → the six convenience functions and their in-module suite
- `stream.rs` (planned) → the per-stream state, the ref-handle registry, the pump, the
  stash, the controller checks
