# `examples/` — the crate's measurement bench binaries

## Intent
The in-process A/B and micro-benchmark binaries behind the performance claims recorded elsewhere
(`../../../tools/bench/RESULTS.md` owns the workspace-level figures; these examples produce the crate-local
ones). They measure a change against a **frozen copy of the previous implementation** kept in the same file,
so an A/B needs no second checkout.
Host discipline is binding here: this host is shared, so absolute wall times are noise at these effect sizes —
read the **retired-instruction count** or a ratio between two measured quantities, and report
`/proc/loadavg` (each binary prints it) alongside the numbers.

## API Surface
| Example | Measures |
|---|---|
| `atom_fork_bench.rs` | the two whole-table `AtomTable` copies in retired instructions: `atom_fork_bench <none|clone|fork> [size] [iters]`, where `none` is the no-copy control so `instructions/op = (mode − none)/iters`; measures one copy at a size the boot actually reaches (the per-module load pays it once per module) |
| `atom_witness.rs` | `snapshot()`/`generation()`/`appended_since()` against the O(table) `AtomTable::clone()` it replaces: the witness columns must stay **flat** in table size while `clone` grows linearly |
| `map_build_bench.rs` | `map::write_map` against a frozen copy of the pre-change (per-insertion-scan) builder, over a compiler-shaped corpus; both sides write into the **same** pre-allocated buffer and consume the **same** pre-built `pairs` — so what is timed is the builder, with no output allocation, no input construction and no BEAM heap |
| `cmp_lookup_bench.rs` | one bucket per run of the comparison/lookup path (`flat32_atom`, `flat32_int`, `hamt512_{atom,int}`, `payload_eq`, `payload_hash`, `bin_eq`, `tuple_eq`, `cmp_atoms`); shaped for `perf stat -e cpu_core/instructions/u` / `perf record` attribution: `--example cmp_lookup_bench -- <bucket> [ops]` |
| `size_walk_bench.rs` | the sizing walk three ways over the **same** graph: `retired` (frozen pre-change walk: per-call `Vec` + SipHash path set), `fresh` (live walk through the scratch-free entry point — the difference is the hasher alone), `scratch` (live walk on one warm `SizingScratch`); interleaved per call with a rotating order over five buckets, reporting ns/call, both ratios, deterministic per-call allocation counts and load before/after |

## Constraints
- Run them with `erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml -p beam_term
  --example <name> [args]` (never `cd`).
- A frozen `mod retired` copy inside an example is the **A/B oracle**; it is not dead code and must not be
  "cleaned up" into the live implementation.
- Every figure these produce is a measurement, not a test: nothing here runs in CI and nothing here asserts.

## Routing Table
- `atom_fork_bench.rs` → `clone` vs `fork` in retired instructions
- `atom_witness.rs` → the O(1) witness vs the O(table) clone
- `map_build_bench.rs` → `write_map` builder A/B
- `cmp_lookup_bench.rs` → comparison/map-lookup/hash micro-buckets for `perf` attribution
- `size_walk_bench.rs` → the sizing walk A/B (retired / fresh / warm scratch)
