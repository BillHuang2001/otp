# The `copy_bound/etf_roundtrip` cost — where it goes, and where the codec fits

This note decomposes the `copy_bound/etf_roundtrip` benchmark case's cost, separates this crate's share from the runtime's, and records the sibling handoffs for the rest.
Every figure below is committed evidence, never a fresh single run.
The case builds a fixed payload, runs `term_to_binary/1`, `binary_to_term/1` and `=:=` on the result, N times (the Erlang module is `../tools/bench/src/workloads/copy_bound.rs::module_source`).

## 1. The published headline is a bench-harness artifact

m14 (`../tools/bench/RESULTS.md` §3, row `copy_bound/etf_roundtrip`) publishes rust **109 917.8 ns/op** against sys **806.7 ns/op**, i.e. **136.3×**.
That marginal is not a per-op cost: it is the harness's non-interleaved startup drift divided by `N`.
`execute_case` (`../tools/bench/src/workloads/copy_bound.rs:471-570`) runs **all** N-op samples first (Pass 1, ~:490-530) and **all** 0-op samples second (Pass 2, ~:532-567), never interleaved.
With `n_rust = 40 000` (the m14 loop count, `RESULTS.md` §4) the timed loop does only ~62 ms of real work, while rust boot is ~431 ms (1.4–2.5 s on the reference host, `RESULTS.md` §4) and drifts by ≈±0.1 s run to run.
The published marginal is that drift, not the loop.
The startup-cancelled marginal guard (`ns_per_op`, `../tools/bench/src/workloads/copy_bound.rs:435-445`) rejects only a **negative** marginal (one below timer resolution), so a hugely **positive** inflated marginal ships unguarded.

## 2. The reproducible ratio

Measured interleaved, min-of-5, N = 1e6 (loadavg ~3.9→5.1): rust **1546.5 ns/op** against sys **640.1 ns/op** = **2.42×**.
A live driver rerun gave **1750.1 ns** against **657.7 ns** = **2.66×**.
Linear scaling 1561 ns @1e5 → 1541 ns @1e6 confirms the per-op cost is ~1.5–1.75 µs, i.e. stable across `N` in a way the published figure is not.
The fair ratio is therefore **~2.4–2.7×**, not 136.3×.

## 3. Per-term decomposition of that rust 1546.5 ns/op (the pre-optimization baseline)

- `payload/0` build ≈ 25–60 ns (sys ≈ 2 ns) — runtime, not the codec.
- `term_to_binary(payload)` ≈ 390 ns (sys 207 ns): **codec-alone encode ≈ 148 ns**, so the `beam_bifs`/`beam_rt` glue around it is ≈ 242 ns.
- `binary_to_term(image)` ≈ 798 ns (sys 380 ns): **codec-alone decode (warm) ≈ 282.5 ns**, so the `beam_bifs`/`beam_rt` glue around it is ≈ 516 ns.
- The `=:=` bound-variable match ≈ 258 ns (sys 39 ns) — `beam_term::compare`, **6.6×** sys.
- **Codec total ≈ 431 ns ≈ 28 %** of the op (encode 148 + decode 282.5); the remaining ~72 % is the runtime's `payload/0`, the BIF layer and the match.

## 4. The codec's cost now (post-optimization)

Two codec optimizations landed; both are pinned deterministically by `tests/alloc_count.rs`.
- **Decode container buffers are pre-sized** from the wire count clamped to `const CONTAINER_PREALLOC_CAP = 64` (`src/decode/mod.rs`).
  The deterministic allocation effect: warm decode **4 → 3**, fresh decode **14 → 13**; encode is unchanged at **1**.
- **`encode_list` is single-pass** (`src/encode/mod.rs`), rewinding the sink with `Writer::truncate` / `Writer::patch_u16be` (`src/writer.rs`) instead of buffering; it adds no allocation.
- Controlled interleaved measurement — 3 release binaries built from the same tree, round-robin pinned to core 3, min over n=20 across two rotation orders:

| binary | encode ns | decode warm ns | decode cold ns |
|---|---|---|---|
| before (pristine) | 130.0 | 248.7 | 556.6 |
| A only | 128.8 | 224.5 | 530.8 |
| A + B | 118.8 | 216.4 | 518.9 |

Encode improves ≈ **−8.6 %** (Opt B); decode warm ≈ **−9.7 %**, cold ≈ **−4.6 %** (Opt A).
Step-1 single-run mins are higher in absolute terms (encode 127.8, warm 293.0, cold 649.9) because the host is shared and was heavily loaded — read those as a floor, not a record.
**Measurement discipline: read the ratios and the deterministic counters (`tests/alloc_count.rs`), never one absolute wall time.**

## 5. Handoffs (verified evidence; owners are siblings — not edited here)

### (i) Dominant cost — the `beam_rt` BIF-result copy-out (owner: `beam_rt`)

`erlang:binary_to_term/1` dominates the op (~50 %) and its dominant sub-term is the decode materializing onto the pooled BIF heap and then being deep-copied onto the process heap.
The route is in `../beam_rt/src/context/calls.rs`: `call_registered` (~:214), the foreign-container check `result_is_foreign_container` (~:823), `move_result_into_process_heap` (~:889/:905), the `beam_message::copy_struct` call (~:1202) and the pool rewind `heap.reset_to(mark)` (~:754-766).
Nothing in this crate can remove that copy: the codec writes into the heap the caller gives it, under the caller's ownership.

### (ii) The `beam_bifs` encode triple buffer + image copy (owner: `beam_bifs`)

`../beam_bifs/src/binconv.rs` builds a `term_to_binary` image through a triple buffer: `encode_body` (the body `Vec`, `:931`) → a second `plain_image` `Vec` (`:999`) → `make_bin` (`:271`).
Each decode then copies the whole image into a `Vec` through `bytes_arg` (`binconv.rs:262`), which forwards to `bits::bytes` (`bits.rs:142`) and `access::binary_bytes` (`access.rs:513`).
Nothing in this crate can remove those copies: the codec is handed a byte slice and returns another.

### (iii) The `tools/bench` non-interleaved-pass harness artifact (owner: `../tools/bench`)

The non-interleaved Pass 1 / Pass 2 split in `execute_case` (section 1) is what turns the real ~2.4–2.7× into the published 136.3×.
Fixing it — interleaving the N-op and 0-op draws, or rejecting a marginal far larger than the runtime's own noise band — belongs to the bench harness.

This crate's share of the case is characterized above; each remaining fix belongs to the named sibling.
