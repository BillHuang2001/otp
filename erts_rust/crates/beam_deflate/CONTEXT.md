# `beam_deflate` — the workspace's one zlib port (DEFLATE / INFLATE)

## Intent
A safe-Rust port of the **zlib 1.3.2** the repository already ships (`../../../erts/emulator/zlib/`),
owned by this crate so that both `beam_codec` (ETF `COMPRESSED`) and `beam_bifs` (`zlib` module BIFs)
consume **one** implementation instead of two diverging encoders.

Why a port rather than a dependency: `zlib`'s output is an observable. `zlib:compress/1`,
`zlib:zip/1`, `zlib:gzip/1`, the `zlib` stream API and ETF's `COMPRESSED` tag all return zlib's byte
string to the user, so byte identity with the oracle is the contract, not "a decodable stream".
`flate2`'s `rust_backend` (miniz_oxide) is a different algorithm — it agrees with zlib on short
literal-only bodies and diverges as soon as a match is emitted or a block is split — and
`libz-sys` is unavailable in this workspace's offline dependency cache. `flate2`'s `zlib-rs` backend
is zlib's *interface*, not its output (it picks `deflate_medium` for levels 3–6 where zlib uses
`deflate_slow`).

This crate is **bytes only**: no terms, no BIF registry, no host state, no `unsafe`.

## API Surface
- One-shot: `compress` (zlib-wrapped, `compress2`), raw/gzip framings, `decompress` with a bound and
  the consumed-input count.
- Streaming: a `deflate` stream and an `inflate` stream with zlib's own status codes
  (`Ok`/`StreamEnd`/`NeedDict`/`BufError`/`DataError`/`StreamError`) and `avail_in`/`avail_out`
  accounting, so a caller can reproduce zlib's per-call output chunking exactly.
- Checksums: `crc32`, `adler32` and their `combine` forms (RFC 1950/1952 trailers).

## Constraints
- Safe Rust only (`#![forbid(unsafe_code)]` or an equivalent crate-level rule): no `unsafe` block.
- No external dependencies beyond `beam_common` for `Result`/`BeamError`.
- Apache-2.0; the OTP `// %CopyrightBegin% … // %CopyrightEnd%` block on every `*.rs` file.
- Files target ~1000 lines; split by responsibility (`deflate/`, `inflate/`, `checksum.rs`).
- **Byte identity or a typed error — never a conforming approximation.** Where a zlib parameter
  combination is not ported, the entry point returns a typed `BeamError`, and the crate `CONTEXT.md`
  records exactly which combinations are exact.
- Ground truth is read-only: `../../../erts/emulator/zlib/` (zlib 1.3.2),
  `../../../erts/emulator/nifs/common/zlib_nif.c` (the NIF layer), and
  `../../../erts/preloaded/src/zlib.erl` (the Erlang wrappers). The oracle is the system
  `erl` (OTP 29.1.1 / erts-17.1) on `PATH`.

## Notes for Agents
- Build/test with `erts_rust/scripts/cargo.sh build|test --manifest-path erts_rust/Cargo.toml -p beam_deflate`
  from the repository root (the sandbox `$HOME` is read-only; the wrapper sets a writable `CARGO_HOME`).
  Always pass an explicit long timeout — a killed call leaves truncated object files.
- The crate sits below `beam_codec` and `beam_bifs`; neither may be depended on from here.

## Routing Table
- `src/deflate/` → the compression direction: the sliding window / hash-chain `deflate_state`, the
  `deflate_stored`/`deflate_fast`/`deflate_slow`/`deflate_rle`/`deflate_huff` strategies, the Huffman
  construction and block emitter, the driver (`deflate()`, flush modes, `deflateParams`).
- `src/inflate/` → the decompression direction: the bit reader, the code-table builder, the
  fast/slow inflate loops, the header/trailer state machine, the dictionary protocol.
- `src/checksum.rs` → `crc32`, `adler32` and the `*_combine` forms.
- `tests/` → oracle-cross-checked integration tests (byte identity both ways, error shapes).

## See Also
- `../CONTEXT.md` → the crate layer's binding cross-crate contracts and routing table.
- `../../../erts/emulator/zlib/CONTEXT.md` → the vendored C being ported.
