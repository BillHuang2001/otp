# m12 — the `run`-route latency defect: **OPEN** (measured, not fixed)

A deliberately **OPEN** regression record for the m12 milestone. It is a `beam_rt` runtime defect, **not** a
defect of any target project, and it is being fixed in parallel by the `beam_rt` owner (root cause and
commits named below). This file measures it; it does **not** fix it, and no metric value in any
`results/*.json` artifact is altered by it.

**Both sides named throughout:** `HEAD` = `beam_rt` built `--release` from the worktree tree `ee3e426db0`
(m12's measured tree; artifacts committed as `bdadd02333`), default (baseline-JIT) tier. `m10` =
`beam_rt` built `--release` from `b4593818b3` in the detached scratch worktree
`$TMPDIR/m10wt`. Vehicle: direct CLI invocations (one OS process per point), wall clock via `date +%s%N`
and CPU time via bash's `time` (`/usr/bin/time` is absent on this host). Host 1-min loadavg during the
probe window: `10.26 → 8.86` (14 CPUs, shared — absolute times are noisy; the **ratios** and the
**per-op CPU/wall shape** are the signal).

Probe window: `2026-10-15T14:55:59Z → 14:58:53Z`.

## Summary table

| probe | m10 (`b4593818b3`) | HEAD default | HEAD with an empty boot script |
|---|---|---|---|
| `run <mod> f/1 0` startup (fixed cost) | 17 / 18 / 19 ms | **2 111 / 2 101 / 2 084 ms** | 18 / 18 / 18 ms |
| `-version` | — | 2 / 2 / 2 ms | — |
| `-noshell -eval 'halt().'` | 18 / 20 ms | **1 997 / 2 040 / 2 029 ms** | — |
| `-boot bootstrap/bin/start` | 385 / 433 ms (**exit 0**) | **2 065 / 2 093 ms (exit 0)** | — |
| `send_recv/1` N = 2 000 (wall) | 248 ms (0.239 s user) | **53 141 ms (1.249 s user)** | 75 ms |
| blocked-round cost | ≈0.115 ms/round | **≈25.5 ms/round** | ≈0.029 ms/round |
| `send_recv_plain/1` N = 2 000 | 218 ms (exit 0) | **77 586 ms, exit 1** (the 60 s deadline) | 54 ms (exit 0) |
| decode-shaped `etf_roundtrip/1` N = 5 000 | 79 ms | **2 404 ms (0.642 s user)** | 56 ms |
| `ets_copy/1` N = 100 000 | 367 ms | **2 421 ms (0.617 s user)** | — |
| boot RSS baseline | ≈15 MiB | **≈394 MiB** | ≈15 MiB |

## A. Fixed `run`-route startup — the `run` action now drives the full boot script (~2.07–2.13 s)

`beam_rt run <file.beam> <mod>:<fun>/1 0` (the `N=0` point of every startup-cancelled `foreign`/`copy_bound`
case, so this is the fixed cost that every loop workload's marginal now subtracts): HEAD default
**2 111 / 2 101 / 2 084 ms** vs m10 **17 / 18 / 19 ms** vs HEAD with a synthetic **empty** boot script
**18 / 18 / 18 ms**.

The `-boot` probe isolates it: `beam_rt -boot bootstrap/bin/start` now takes **2 065 / 2 093 ms** on HEAD
(vs 385 / 433 ms for m10), and the plain `-noshell -eval 'halt().'` path pays the same fixed cost
(1 997 / 2 040 / 2 029 ms) while `-version` does not (2 ms). So **~2.1 s of fixed startup is added to every
non-`-version` CLI action** — the boot-script drive introduced by commit **`85ee33b2f8`** ("beam_rt: every
non-`-version` CLI action drives the default boot script").

> **m11's `-boot` fact is now OBSOLETE.** The m11 record (`RESULTS.md` §3.10/§6.8) stated that
> `beam_rt -boot bootstrap/bin/start` **exits 3** on `opcode: run: process blocked (no message available)`
> and stops at **directive 18 of 22**, i.e. boot does **not** complete. At m12 HEAD that same command
> **exits 0** in ≈2.07–2.09 s and prints `Erlang/OTP 30 [erts-17.1] [source] [64-bit]` — **boot completes**.
> The updated boot-status fact is recorded in `RESULTS.md` §3.10/§6.8.

## B. The per-blocked-round scheduler cost (~25.5 ms/round, ~2 % CPU) — the defect

`send_recv/1` is the two-process copy case, which **blocks** on each round waiting for the reply. Wall ms /
user s / sys s / exit / stdout, HEAD default:

| `N` | wall ms | user s | sys s | per-op (startup-cancelled) |
|---|---|---|---|---|
| 0 | 2 098 | 0.353 | 0.129 | — (startup) |
| 200 | 7 164 | 0.442 | 0.143 | 25.33 ms/op |
| 400 | 12 274 | 0.531 | 0.148 | 25.44 ms/op |
| 2 000 | 53 141 | 1.249 | 0.154 | **25.52 ms/op** |

The per-round cost is a **constant ~25.5 ms of wall clock with almost no CPU** (N = 2 000 burns 1.25 s user
CPU for 53.1 s of wall — ≈2 % CPU). The same case on m10 is **50 / 80 / 248 ms** for N = 200 / 400 / 2 000
(≈0.115 ms/round **with real work**), and on HEAD **with the empty boot script** it is 27 / 75 ms
(≈0.029 ms/round). **Same binary, ~220× apart** depending only on whether the boot script ran — the defect
is boot-induced, not a `copy_bound` program change.

`send_recv_plain/1` (the delivery-only case, **no** `after` guard) is where it becomes fatal: HEAD default
N = 2 000 → **77 586 ms wall, exit 1, empty stdout** — the harness's 60 s deadline
(`beam_rt: error run: bench_copy_bound:send_recv_plain/1 did not terminate within 60000 ms`). HEAD with the
empty boot script returns in **54 ms** (exit 0, stdout `2000`), and m10 in **218 ms**. That is why the
`--full` cross-runtime run's `copy_bound` rust row reports **3 ok, 0 unsupported (of 4)** with **no**
`send_recv_plain` metric, and why the `send_recv` case reads **25 632 422.23 ns/op** (`RESULTS.md` §3.6).

## C. The Θ(atom-table) ETF-decode inflation (the second, independent boot effect)

`etf_roundtrip/1` runs a `term_to_binary` **encode** and a `binary_to_term` **decode** per iteration.
Measured at N = 5 000 (decode-shaped): HEAD default **2 404 ms (0.642 s user)** vs HEAD empty-boot
**56 ms** vs m10 **79 ms**. `ets_copy/1` at N = 100 000: HEAD default **2 421 ms (0.617 s user)** vs m10
**367 ms**. Both runs are `ok` with correct stdout, so this is latency, not correctness.

Mechanism: booting loads all preloaded modules, growing the **shared atom table ~70 → ~4 100**; the
per-decode `prefix_matches` validation is **Θ(table)**, so `binary_to_term` inflates while `term_to_binary`
(encode) is unaffected. This is what makes the `copy_bound` `etf_roundtrip` rust row read
**35 369.21 ns/op** at m12 (vs **7 033.39** at m11) — see `RESULTS.md` §3.6/§7.

## Root cause and ownership (owner: `beam_rt`, **outside this node**)

Both effects come from **booting `start.boot`** on HEAD (commit `85ee33b2f8`, an ancestor of m12 HEAD):

* **Per-op 25 ms scheduler sleep.** The booted kernel's `os:set_signal/2` self-pipe makes
  `PortRegistry::has_nothing_to_poll()` permanently false, so `Runtime::is_quiescent()` never holds and
  **every blocked round sleeps the full `PORT_POLL_SLICE` = 25 ms** (`crates/beam_rt/src/runtime/scheduler.rs`,
  `port.rs`). Hence `send_recv` is a constant 25.5 ms/op at any `N`.
* **Θ(atom-table) decode.** Booting grows the shared atom table ~70 → ~4 100, making `binary_to_term`'s
  `prefix_matches` validation Θ(table) (`beam_bifs/src/binconv.rs` + the mirrored `beam_rt` hosts).

Already documented in-tree at `erts_rust/crates/beam_rt/src/runtime/CONTEXT.md:150` and in
`results/b55-foreign-regression-fix.md` ("Follow-ups left open"). The **separate, already-fixed** issue
(the Θ(N³) `foreign` per-slice off-heap registration) is the subject of `results/m12-foreign-regression.md`
and its fix commits `07a5720639` + `7d84b12b44`.

**Attribution is unambiguous and NOT to the target projects:** the target sources are unchanged
(cowboy HEAD `c965d1a`, emqx HEAD `02eaa91831`), and the *same* HEAD binary with a synthetic empty boot
script removes essentially all of it (53 141 → 75 ms on `send_recv` N = 2 000; 77 586 → 54 ms on
`send_recv_plain`; 2 404 → 56 ms on the decode case). The bench crate is a pure observer; the target
projects' own toolchains are untouched.

## Consequences visible in the m12 artifacts (all recorded honestly, no value altered)

* `results/m12-bench-xrun.json` rust row `baseline startup_ms` = **2 298.63 ms** (vs m11's 15.21 ms) — the
  fixed boot cost, not a `baseline` change.
* `slice` rust walls **2 098–2 367 ms** per case (one process per case ⇒ one boot each).
* `copy_bound` rust: `send_recv` **25 632 422.23 ns/op**, `etf_roundtrip` **35 369.21 ns/op**,
  `ets_copy` **2 883.75 ns/op**, `send_recv_plain` **failed (60 s deadline)** — `3 ok, 0 unsupported (of 4)`.
* Every loop workload's rust peak RSS reads **≈394–472 MiB** (vs ≈15 MiB at m11) — the boot's resident
  preloaded-module set, not a loop-retention change (`RESULTS.md` §6.7 still describes the *separate*
  per-op retention that the loop rows also carry).
* `-boot bootstrap/bin/start` **exits 0** (m11: exit 3) — boot now completes; the remaining A.7 gap is
  fidelity (no self-hosted build), not completion, and is tracked by the differential gate, not here.
