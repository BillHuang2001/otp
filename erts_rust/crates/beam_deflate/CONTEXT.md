# `beam_deflate` — the workspace's one zlib port (DEFLATE / INFLATE)

## Intent
A safe-Rust port of the **zlib 1.3.2** the repository already ships (`../../../erts/emulator/zlib/`),
owned by this crate so that both `beam_codec` (ETF `COMPRESSED`) and `beam_bifs` (`zlib` module BIFs)
consume **one** implementation instead of two diverging encoders.

Why a port rather than a dependency: `zlib`'s output is an observable.
`zlib:compress/1`, `zlib:zip/1`, `zlib:gzip/1`, the `zlib` stream API and ETF's `COMPRESSED` tag all
return zlib's byte string to the user, so **byte identity with the oracle is the contract**, not "a
decodable stream".
`flate2`'s `rust_backend` (miniz_oxide) is a different algorithm — it agrees with zlib on short
literal-only bodies and diverges as soon as a match is emitted or a block is split.
`flate2`'s `zlib-rs` backend is zlib's *interface*, not its output (it picks `deflate_medium` for
levels 3–6 where zlib uses `deflate_slow`).
This crate is bytes only: no terms, no BIF registry, no host state, no `unsafe`.

## API Surface
Frozen shared surface (`src/lib.rs`):
- `Flush { None, Partial, Sync, Full, Finish, Block }` + `Flush::code() -> i32` (`Z_NO_FLUSH` = 0 … `Z_BLOCK` = 5).
- `Strategy { Default, Filtered, HuffmanOnly, Rle, Fixed }` + `Strategy::code()` (0..=4).
- `Method { Deflated }` + `Method::code()` (8).
- `Status { Ok, StreamEnd, NeedDict(u32), BufError, DataError, StreamError, MemError }` + `Status::is_error()`.
- `Pump { status: Status, consumed: usize, produced: usize }` — one pump step's `avail_in`/`avail_out` delta.
- `DEFAULT_COMPRESSION` (-1), `NO_COMPRESSION`, `BEST_SPEED`, `BEST_COMPRESSION`,
  `DEFAULT_MEM_LEVEL` (8), `MIN_WBITS` (8), `MAX_WBITS` (15), `GZIP_WBITS_OFFSET` (16), `AUTO_WBITS_OFFSET` (32).
- One-shot zlib-wrapped (RFC 1950): `compress(input, level: i8) -> Result<Vec<u8>>`,
  `decompress(input) -> Result<Vec<u8>>`, `decompress_bounded(input, limit) -> Result<Vec<u8>>`,
  `decompress_bounded_used(input, limit) -> Result<(Vec<u8>, usize)>` (the `usize` is **consumed input
  bytes**, excluding anything after the stream — `binary_to_term/2`'s `used`).

Compression (`src/deflate/`):
- `DeflateConfig { level: i8, method: Method, window_bits: i8, mem_level: u8, strategy: Strategy }`,
  `Default`, and `new(level, method, window_bits, mem_level, strategy)` /
  `zlib(level)` / `raw(level)` / `gzip(level)`.
- `GzHeader { text, hcrc, time, os, extra, name, comment }` + `with_time(u32)` (`deflateSetHeader`).
- `Deflate::{new(DeflateConfig), compress_with(DeflateConfig, &[u8]), deflate(&mut self, input, output, flush) -> Result<Pump>, reset(), set_dictionary(&[u8]) -> Result<u32>, set_header(GzHeader), set_params(level, strategy, output) -> Result<Pump>, config()}`.

Decompression (`src/inflate/`):
- `Inflate::{new(window_bits: i8), inflate(&mut self, input, output, flush) -> Result<Pump>, reset(), reset_with_window_bits(i8), set_dictionary(&[u8]), get_dictionary() -> &[u8], at_stream_end() -> bool, window_bits() -> i8}`.
- `decompress_all(window_bits, input, limit) -> Result<(Vec<u8>, usize)>` — bounded one-shot loop; a
  `NeedDict` answer becomes `BadArg` because the one-shot signature has no dictionary parameter.

Checksums (`src/checksum.rs`): `adler32`, `crc32`, `adler32_combine`, `crc32_combine`
(all re-exported at the crate root) — the single `erts_refc`-style primitive for every trailer in the
workspace; `beam_codec`/`beam_bifs` must use these, never a local copy.

## Exact-vs-typed matrix
**Byte-identical to the oracle (system `erl`, OTP 29.1.1 / erts-17.1)** — every row was measured, not
assumed; the oracle figures live in `tests/deflate_goldens.rs`, `tests/deflate_oracle.rs`,
`tests/inflate_goldens.rs`, `tests/inflate_oracle.rs`:

| Parameter | Accepted (byte-exact) | Typed error (`BeamError::BadArg`) |
|---|---|---|
| `level` (deflate) | `-1` (= 6) and `0..=9` | `< -1`, `> 9` — the C's `Z_STREAM_ERROR` |
| `strategy` | `Default`/`Filtered`/`HuffmanOnly`/`Rle`/`Fixed` | — (a Rust enum has no other value) |
| `window_bits` (deflate) | `8..=15` zlib (8 is bumped to 9, as the C does), `-15..=-9` raw | `-8` and `16 + 8 = 24` (the C's `(windowBits == 8 && wrap != 1)` clause), `<-15`, `>31`, `16`, `32+` |
| `window_bits` (deflate) | `25..=31` gzip (`16 + 9 ..= 16 + 15`) | see above |
| `mem_level` | `1..=9` | `0`, `> 9` |
| `flush` | `None`, `Partial`, `Sync`, `Full`, `Finish`, `Block` | — |
| `window_bits` (inflate) | `0` (from the zlib header), `8..=15` zlib, `-15..=-8` raw, `24..=31` gzip, `40..=47` auto zlib/gzip | `-7..=-1`, `1..=7`, `17..=23`, `33..=39`, `>= 48` — the C's `Z_STREAM_ERROR` set |
| block types (inflate) | stored, fixed, dynamic | malformed trees/codes/trailers ⇒ `Status::DataError` |

Notes that matter to a consumer:
- **Level 0 is pump-schedule dependent** (`deflate_stored` sizes its blocks from `avail_out`, `pending_buf_size`
  and `w_size`, `deflate.c::deflate_stored`). `Deflate::compress_with` reproduces the **`zlib_nif.c` schedule**
  (`?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE` = 8 KiB/8 KiB, one `deflate(Z_NO_FLUSH)` per input piece
  then one `deflate(flush)`), which is the schedule the Erlang caller sees through `zlib:deflate/3`.
  Levels 1–9 are schedule-invariant (verified: 1-byte/7-byte/1 KiB output buffers and 1-byte/3-byte/1024-byte
  input pieces reproduce the one-shot image exactly). `tests/deflate_stream.rs` pins both facts.
- **ETF `COMPRESSED` only ever uses levels 1..=9**: `erts/emulator/beam/external.c` skips zlib entirely at
  `{compressed, 0}` and emits the uncompressed image, and `[compressed]` is `Z_DEFAULT_COMPRESSION` (6).
  A `beam_codec` implementation must therefore **not** call `compress(.., 0)` for that option.
- `Partial`/`Block` have no `zlib.erl` binding (`arg_flush/1` exposes only `none`/`sync`/`full`/`finish`), so they
  are covered structurally (`Z_BLOCK` output is a strict prefix of `Z_SYNC`; `Z_PARTIAL` extends `Z_BLOCK` by
  the alignment byte) plus round-trip, not by byte goldens.
- `Z_FIXED` and the rejected `windowBits` values cannot be expressed through `zlib.erl`, so they are
  oracle-unverifiable by construction; `Z_FIXED` is covered by a round trip through `crate::inflate`.

## Design Decisions
- **`deflate_state` is reproduced field-for-field** (`src/deflate/state.rs`) because every counter feeds a
  heuristic: `strstart`/`lookahead`/`insert`/`block_start`/`high_water`, the hash-chain discipline, the
  `uInt`-wrapping of `match_start`, `configuration_table`'s `good`/`lazy`/`nice`/`chain`.
- **Deviations from the C that cannot change a byte** (each documented at its definition): `pending_buf` is a
  growable `Vec` (an invariant slip is a leak, not an out-of-bounds write) with `pending_buf_size` still
  computed and read by `deflate_stored`; `sym_buf` is a separate `Vec` instead of the C's overlay on
  `pending_buf` (a memory optimisation only); `ct_data` keeps `freq`/`code`/`dad`/`len` as separate fields
  (`init_block` zeroes every `freq`, so `Len == 0 ⇒ Code == 0` and `send_bits(_, 0)` is a no-op); `slid` is
  dropped (only the unported `deflateCopy` reads it); `opt_len`/`static_len` are `u64` with wrapping
  arithmetic (only `(opt_len + 10) >> 3` is read).
- **Inflate uses a *mirrored* window** (`window[i] == window[i + wsize]`, `2 * wsize` bytes): `wsize`/`whave`/
  `wnext` keep the C's meaning, the C's two-run `inflateGetDictionary` copy and the three `wnext` match-copy
  wrap cases disappear, and the returned dictionary is the same oldest-first byte string.
- **`inflate_table` is the C's algorithm, not a simpler decoder** — same over-subscribed/incomplete/missing-
  end-of-block acceptance rules; three separate table arrays instead of the C's shared `codes[ENOUGH]` arena
  (no output depends on table placement); `inffixed.h` is data, proven equal to the builder by a unit test.
- **Not modelled in inflate** (none is read back by this crate or by `zlib_nif.c`): `strm->msg`, `data_type`,
  `inflateGetHeader`/`head`, `back`/`was`/`inflateMark`, `sane`, the `Z_TREES` stopping points.
  `PKZIP_BUG_WORKAROUND`/`INFLATE_STRICT`/`INFLATE_ALLOW_INVALID_DISTANCE_TOOFAR_ARRR` are undefined in
  OTP's build and are not compiled in here either.
- **Error strategy.** Stream-level outcomes are `Status` values (a malformed stream is
  `Ok(Pump { status: DataError, .. })`, exactly as the C returns `Z_DATA_ERROR`); `beam_common::Result` is
  reserved for a rejected configuration, i.e. the cases where `deflateInit2_`/`inflateInit2_` return
  `Z_STREAM_ERROR` before any byte is processed. Nothing panics on data.

## Consumer shapes (what `beam_codec` / `beam_bifs` should call)
- **ETF `COMPRESSED` (`beam_codec`)**: `compress(body, level)` for levels 1..=9 (and `-1`), and
  `decompress_bounded_used(stream, declared_inner_size)` — the consumed count is the `used` option and the
  bound is the C's "fill the declared buffer completely" rule. Never compress at level 0 (see the matrix).
- **The `zlib` BIF layer (`beam_bifs`)**: drive the **pump**, reproducing
  `erts/emulator/nifs/common/zlib_nif.c::zlib_flush_queue` + `zlib_codec`: one `output_chunk_size` buffer;
  per queued input piece `deflate/inflate(Z_NO_FLUSH)` summing `consumed`; when the queue drains and
  `produced < output_chunk_size` one more call with an empty input and the caller's `flush`; classify
  `{finished, Out}` / `{continue, Out}` / `{need_dictionary, Adler, Out}` in the C's order.
  `deflate/3` uses 8192/8192, `inflate/2` and `safeInflate/2` 8192/16384 (`erts/preloaded/src/zlib.erl`).
  `tests/roundtrip.rs` contains a faithful re-implementation of that loop as executable evidence.
- `deflateParams` maps to `Deflate::set_params`: the Erlang layer first calls `deflate(Z, <<>>, sync)`; with an
  empty input and a 1-byte dummy output the C returns `Z_OK` (or `Z_BUF_ERROR` when the pending block does not
  fit) and only `Z_STREAM_ERROR` is propagated, so `set_params` returns `Status::Ok`/`BufError` accordingly.
- `zlib:inflateInit/1,2` maps to `Inflate::new(window_bits)`; the NIF layer's `eos_behavior`
  (`error`/`reset`/`cut`) is the **BIF layer's** policy (it calls `reset` after `StreamEnd`), not a
  `beam_deflate` concept. `zlib:deflateSetHeader` has no `zlib.erl` binding; `Deflate::set_header` exists for
  gzip-member fidelity.

## Test Strategy
`cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_deflate` — **140 passed / 0 failed / 5 ignored**
(~40 s on this host, the oracle leg dominating), warning-free, safe Rust only (`#![forbid(unsafe_code)]`).

| Target | What it pins |
|---|---|
| `src/**` unit tests (55) | constants/tables, `longest_match`, the bit writer, `fill_window`, `build_tree`/`gen_bitlen`/`gen_codes`, `inflate_table` acceptance rules, window/dictionary, fixed tables, state/reset |
| `tests/deflate_goldens.rs` (5 + 1 ign) | embedded oracle bytes: levels `-1,0..=9` × 5 inputs (zlib), levels `0,1,6,9` × 5 inputs (raw, gzip); 12 `(len, crc32)` fingerprints for the 70 000-byte input; `#[ignore]d regenerate_goldens` refuses to run unless the port already agrees with the oracle |
| `tests/deflate_oracle.rs` (9) | live oracle: levels × framings, strategies, every `memLevel`, every `windowBits` form, large-input round trips, rejected configurations |
| `tests/deflate_stream.rs` (8 + 1 ign) | output-buffer/input-piece independence, `Sync`/`Full` split goldens with the `00 00 FF FF` alignment, `Block`-is-a-prefix-of-`Sync`, `set_params` 6→1 and 1→9 goldens, level-0 schedule dependence |
| `tests/inflate_goldens.rs` (15) | 38 embedded oracle images across all framings, chunking invariance, `consumed`, the output bound |
| `tests/inflate_bits.rs` (11) | header/trailer/error shapes, per-offset truncation (never a panic), concatenated gzip members via `reset`, auto-detect |
| `tests/inflate_oracle.rs` (7) | live oracle: 1:1 `safeInflate` chunk sequence at 16 KiB, per-prefix production counts, dictionary protocol, framings, reset |
| `tests/inflate_edge.rs` (9) | hand-built fixed-Huffman stream at the maximum distance 32768, distance-past-decoded-bytes, `get_dictionary` after partial decodes (incl. a 9-bit window), `reset_with_window_bits` re-framing, `BufError`/`Ok`/`StreamEnd` boundaries |
| `tests/lib_api.rs` (11 + 1 ign) | the one-shot/bounded contract: oracle images per level, `BadArg` for bad levels and malformed streams, the consumed count, the inclusive bound, the `usize::MAX` path on a 4 MiB payload, 440 round trips |
| `tests/roundtrip.rs` (10) | the `zlib_nif.c` pump loop re-implemented, 198-case matrix, incremental `none`+`finish`, sync flush as a resumption point, `safeInflate` loop, `set_params` after a sync pump, preset dictionary through both directions |
| `tests/corpus.rs` (2 ign) | the on-disk corpus sweep (`BD_CORPUS` → `BD_OUT`; `BD_ORACLE_IN`/`BD_ORACLE_OUT` comparison leg) |

Oracle tests **skip loudly** (`SKIP …: erl not found`) and never fail when `erl` is absent, so CI without an
Erlang install stays green; every `erl` invocation goes through `zsupport::oracle_lock()` (one boot at a time;
parallel boots are what made early runs flaky). Oracle inputs are handed over through files, never `argv`
(a 70 KiB stream hex-encodes past `E2BIG`).

## Constraints
- Safe Rust only (`#![forbid(unsafe_code)]`); no dependencies beyond `beam_common`.
- Every fallible API returns `beam_common::Result<T>`; no crate-local error enum; no `unwrap()`/`expect()`/
  `panic!` outside `#[cfg(test)]`; no panic-as-control-flow.
- **Byte identity or a typed error — never a conforming approximation.** Where a parameter combination is not
  ported, the entry point returns `BeamError::BadArg`, and the exact-vs-typed matrix above is the record.
- Apache-2.0; the OTP `// %CopyrightBegin% … // %CopyrightEnd%` block on every `*.rs` file (byte-exact;
  the repo scanner checks it). `CONTEXT.md`/`Cargo.toml` are the crate's only non-`.rs` files and are out of
  that scanner's scope.
- Files target ~1000 lines. Two are legitimately longer and one-to-one with the C they port:
  `src/inflate/pump.rs` (1166, `inflate.c`), `tests/deflate_goldens.rs` (1062, embedded oracle bytes),
  `src/deflate/trees.rs` (895, `trees.c`), `tests/roundtrip.rs` (941, the pump-loop harness + matrix).
- Build/test from the repository root, never `cd`:
  `erts_rust/scripts/cargo.sh build|test --manifest-path erts_rust/Cargo.toml -p beam_deflate`
  (the sandbox `$HOME` is read-only; the wrapper sets a writable `CARGO_HOME`). Pass a long timeout
  (600000 ms+) — a killed call leaves truncated object files.
- Ground truth is read-only: `../../../erts/emulator/zlib/`, `../../../erts/emulator/nifs/common/zlib_nif.c`,
  `../../../erts/preloaded/src/zlib.erl`. The oracle is the system `erl` on `PATH`
  (`ERTS_ORACLE` overrides the program name, as in the differential harness).

## Routing Table
- `src/lib.rs` → the frozen shared surface: `Flush`/`Strategy`/`Method`/`Status`/`Pump`, the level/window
  constants, and the one-shot zlib-wrapped free functions.
- `src/checksum.rs` → `adler32`/`crc32` and both `*_combine` forms (the workspace's only checksum port).
- `src/deflate/` → the compression direction (zlib's `deflate.c` + `deflate.h` + `trees.c`):
  - `mod.rs` → `DeflateConfig`/`GzHeader`/`Deflate` (the public stream API) and the one-shot `compress_with`.
  - `state.rs` → `DeflateState` (`deflate.h`: window, hash chains, symbol buffer, `configuration_table`).
  - `trees.rs` → Huffman construction + the block emitter (`trees.c`), `lz77.rs` → the five strategy
    functions + `longest_match`, `window.rs` → the `Io` cursor / `read_buf` / `fill_window`,
    `driver.rs` → the `INIT`/`BUSY`/`FINISH` machine, headers/trailers, `flush_pending`,
    `tables.rs` → `trees.h`'s constant tables.
- `src/inflate/` → the decompression direction (zlib's `inflate.c` + `inftrees.c` + `inffast.c`):
  - `mod.rs` → `Inflate` (public) + `decompress_all`; `pump.rs` → `inflate()` and `inflateSetDictionary`;
    `inftrees.rs` → `inflate_table`; `inffast.rs` → `inflate_fast` + `copy_match`; `inffixed.rs` →
    `inffixed.h` as data; `state.rs` → `inflate_state` + `inflateReset`/`inflateReset2`; `window.rs` →
    the mirrored window, `updatewindow`, `get_dictionary`.
- `tests/` → oracle-cross-checked integration suites + `tests/zsupport/` (the `erl` oracle harness:
  batched one-boot matrices, the per-process scratch dir, the oracle mutex).

## Known Issues
- Level 0's byte image depends on the pump schedule (see the matrix); only the `zlib_nif.c` schedule is
  reproduced, deliberately.
- The `Partial`/`Block`/`Z_FIXED`/rejected-`windowBits` rows cannot be compared with the oracle through
  `zlib.erl`; they are covered structurally and by round trip.
- `decompress_all` cannot answer `NeedDict`: a dictionary-bearing stream is `BadArg` through the one-shot
  functions — a caller that supports dictionaries must drive `Inflate` (as `beam_bifs` will).
- `beam_codec` still carries its own `flate2`-based `compress`/`decompress_bounded_used`
  (`crates/beam_codec/src/compress.rs`); switching it to this crate is the parent's round, and its
  `MAX_UNCOMPRESSED` policy stays a `beam_codec` decision (this crate takes the bound from the caller).

## See Also
- `../CONTEXT.md` → the crate layer's binding cross-crate contracts and routing table.
- `../../../erts/emulator/zlib/CONTEXT.md` → the vendored C being ported.
- `../../../erts/emulator/nifs/common/zlib_nif.c` and `../../../erts/preloaded/src/zlib.erl` → the
  Erlang-facing layers whose pump protocol and per-call chunking this crate must (and does) reproduce.
- `../../../erts/emulator/beam/erl_zlib.c` + `external.c` → the ETF `COMPRESSED` shape
  (`erl_zlib_compress2` = `deflateInit` + one `deflate(Z_FINISH)`; level 0 emits no zlib stream at all).
