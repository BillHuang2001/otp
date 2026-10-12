# `epp` on the scheduled (`run`) route — localization findings

Status: **localized to the delivery-driven major collection; NOT fixed in `beam_rt`.** The divergence is
caused by a **scheduled process's live mature-resident term being freed by a collection that does not
reach it**, not by the file-server seam, the ETF codec, the message copier or `beam_rt`'s published root
set. All instrumentation used to reach this conclusion has been removed from the tree.

## 1. Reproduction (deterministic)

`b13:go/0` writes `b13_in.erl` (`-module(b13_in). -export([f/0]). f() -> 1.`), reads it back and calls
`epp:parse_file(File, [], [])`, then prints `NFORMS=length(Forms)`.

```
erts_rust/target/debug/beam_rt -noshell run <dir>/b13.beam b13:go/0     # NFORMS=6  (bug)
erts_rust/target/debug/beam_rt -noshell -pa <dir> -eval 'b13:go()'      # NFORMS=5  (correct)
erl -noshell -pa <dir> -eval 'b13:go()' -s init stop                    # NFORMS=5  (oracle)
erts_rust/target/debug/beam_rt -noshell +hms 500 run <dir>/b13.beam b13:go/0   # NFORMS=5 (masked)
```

A small nursery (`+hms 233`, the default here) is what makes the divergence appear; the repro is
deterministic. The `-eval` route is a single non-scheduled pass and never diverges.

## 2. The decisive measurement — address vs contents at the send seam

The probe printed, at the delivery seam (`RuntimeContext::deliver_message_with_off_heap_inner`, i.e. on
the **sender's own term, before any copy**), the raw address `ptr_val(XtraArgs[1])` — the scan
`Options`, which is the `epp` server's `#epp.erl_scan_opts` list
(`lib/stdlib/src/epp.erl:1048-1050`, used at `:1324`) — alongside its contents and the region the
address lies in for the sending process.

| Probe | `opts` address | contents | region at that moment |
|---|---|---|---|
| `OPT[26]` | `0x…d6bf80` | `[0x…d6bf92 \| 0x…d6bfa9]` intact | `mature` (pid3 mature base `0x…d6bee0`, used 39) |
| `OPT[30]` | same `0x…d6bf80` | intact | `OTHER` |
| `OPT[34]` | same `0x…d6bf80` | intact | `OTHER` |
| `OPT[38]` | same `0x…d6bf80` | `[0x2580b \| 0x2f]` **CLOBBERED** | `OTHER` |

**The address is byte-stable while the contents degrade** ⇒ the term is **never relocated**; a
collection frees the block that holds it and a later allocation overwrites it in place. The `mature`
→ `OTHER` transition is exactly the moment the sending process's mature space was replaced.

## 3. The collecting site is pinned — `Process::deliver`, not a `beam_rt` admission door

Instrumenting `beam_rt`'s four admission doors (`collect_or_grow` / `ensure_heap` / `ensure_heap_full` /
`alloc_words_rooted` / `Runtime::serve_gc_request`) produced **no** line in the divergence window. The
one collection that matters is the delivery-driven one:

```
DELIVER-START to=pid3 sender=pid2 (state=Waiting, exempt=false, unpub=false)
GC before-deliver-to pid3 young(base=…6120 used=233 cap=233 nspaces=1) mature(base=…7ee0 used=39) minor=0
     ROOT[3] = 0x…27c2 class=cur      # x[3]
     ROOT[6] = 0x…27da class=cur      # x[6]
     ROOT[7] = 0x…2779 class=cur      # x[7]
     ROOTCOUNT n=2053 in-heap=3
DELIVERED
GC after-deliver-to  pid3 young(base=…6120 used=4   cap=233 nspaces=1) mature(base=…19c0 used=16) minor=0
```

* The collection runs inside `beam_process::Process::deliver` →
  `Process::ensure_capacity` → `run_rooted_collection` (the `minor_gcs` counter stays 0 and the mature
  **base changes**, so this is `Collector::collect_major`, `beam_gc/src/collector.rs:195`).
* It ran with the receiver's `relocation_exempt == false` and `has_unpublished_roots() == false`
  (`beam_process/src/process.rs:1097` = `exempt || state == Running`), so `deliver`'s gate
  (`:936`) let it collect. `beam_rt`'s `root_is_unscanned` valve is **not** consulted at this site.
* **At the collecting moment the receiver's gathered root families contain NO word pointing into the
  mature generation** — `in-heap=3` and every in-heap root is `class=cur` (young). (At a *neighbouring*
  park — `ROOT[0]`, `stack[0]`, `stack[3]` = `class=mature` — such roots do exist, so the probe is
  capable of seeing them.) The process nevertheless keeps using a mature-resident term (its
  `erl_scan_opts` field) afterwards.

⇒ The freed term is not reachable from the root families `run_rooted_collection` gathers
(`regs.gather_roots` = x + whole stack + `extra_roots`, mailbox `payload`/`from`/`token`, `dict`,
`saved_args`, `signals`; `beam_process/src/process.rs:1232`), so the major collection sweeps it and
`Nursery::collapse`/`self.mature = fresh` releases its block.

## 4. What this rules out

* **Not the native `file_server`** (`get_until` → `beam_io::io_device::Continuation` →
  `erl_scan:tokens/4`): it returns the right tokens and the right `eof`; the same path driven from the
  main scheduled process agrees with the oracle.
* **Not the message copier**: plain cross-process sends of these shapes agree.
* **Not `beam_rt`'s park/resume publication**: `publish_parked_roots`
  (`src/runner/run.rs:97`) publishes the whole `RootSet::ALL_LIVE` (x + stack + FR) and the pending
  exception via `extra_roots`, and `scatter_roots_full`/`resync_parked_roots` reconcile the machine on
  resume. A range-based scan of every published root against the *voided nursery* blocks found zero
  dangling roots. The finding here is a **mature**-resident term, which that scan did not cover, and the
  gathered roots at the collecting moment hold no mature pointer at all.

## 5. Where the defect is (sibling crate — not this node's write scope)

`beam_process::Process::deliver` collects a receiver whose live mature-resident state is reachable only
through a chain that its gathered families do not cover, while the collection is allowed to run because
the only gate is `has_unpublished_roots()` (`relocation_exempt || state == Running`) — a test that
cannot see a reference the gathered root set does not contain. The collection then frees the live term
(`beam_gc/src/collector.rs:195` `collect_major` → `self.mature = fresh`, `:226`, via
`beam_gc/src/copy.rs::Copier::classify_boxed`/`classify_cons`'s `from.contains` test).

Closing it needs a change in `beam_process` (`Process::deliver` / `run_rooted_collection`) and/or
`beam_gc` (the major collection's from-set/descent), not in `beam_rt`.

## 6. Regression test (blocked on the fix)

`tests/compile_file.rs` must drive `epp:parse_file/3` (or `compile:file/2`) through the scheduled `run`
route on a **self-contained fixture under `tests/fixtures/`** and assert the observable result (parsed
form count 5, or a byte-identical produced `.beam`). `b13.erl` hard-codes an absolute path and is
therefore not committable as-is.
