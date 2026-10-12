# `epp:parse_file/3` on the scheduled (`run`) route — localization findings

Status: **localized, not fixed.** The invalid-form / `error:badarg` divergence is caused by
**the `epp` server process's own heap being clobbered**, not by the file-server seam, the ETF
codec, or a sibling crate's protocol handling. All instrumentation used to reach this conclusion
has been removed from the tree.

## 1. Reproduction (deterministic)

Source `x1_in.erl` (43 bytes):

```erlang
-module(x1_in).
-export([f/0]).
f() -> 1.
```

`go/0` calls `epp:parse_file(File, [], [])`, renders every returned form with
`io_lib:format("~w~n", [F])` into a file, with a `try … catch` so a render failure is recorded.

* `beam_rt -noshell run x1.beam x1:go/0` → 6 forms, of which the **`-export` form renders as
  `<<RENDER-FAIL error:badarg>>`** and an **extra `{error,{4,epp,cannot_parse}}`** appears.
* `beam_rt -noshell -pa . -eval 'x1:go()'` → the 5 oracle-identical forms, no error.

The bad form is not an Erlang term of any type (`is_tuple/is_list/is_atom/…` all `false`,
`element/1` fails, `io_lib:format("~w", …)` raises `badarg`,
`erlang:term_to_binary(F, [])` raises the codec's generic unknown-tag
`unsupported_external_term`).

Minimal sub-reproducers (same module, `run` route only):

| Probe | Shape | Outcome on `run` |
|---|---|---|
| `epp:scan_erl_form/1` in a loop | renders each token list | tokens **correct**, but an extra device error `{error,{4,epp,cannot_parse}}` where the oracle has `{eof,4}` |
| manual `epp:scan_erl_form/1` + `erl_parse:parse_form/1` | renders tokens **and** form | **everything correct** (allocation timing matters) |
| `epp:parse_erl_form/1` loop | nothing rendered between steps | epp server **dies** (`beam_rt: term not found` — a later send to a dead pid) |
| `io:scan_erl_form(F, '', N, [])` from the main process | renders results | **correct** — the native file server is fine |
| plain cross-process send of the same tokens/forms | render | **correct** — the copier is fine for these shapes |

## 2. Instrumentation that localized it

An env-gated trace was installed on (a) the nested `erl_scan:tokens/4` call in
`src/native/file_server/get_until.rs`, and (b) the delivery seam
(`RuntimeContext::deliver_message_with_off_heap_inner`) — printing the request's `XtraArgs[1]`
(the scan `Options`) as a raw word sketch, i.e. **on the sender's own term, before the copy**.

Sender-side transcript (`epp:scan_erl_form/1` loop; identical shape for `parse_file/3`), where
`Options` is `[{text_fun, F}, {reserved_word_fun, F}]`:

```
TRACE send cmd=get_until xtra1=List[0x743a875d8702, 0x743a875d8719]   # request 1 — intact
TRACE send cmd=get_until xtra1=List[0x743a875d8702, 0x743a875d8719]   # request 2 — intact
TRACE send cmd=get_until xtra1=List[0x743a875d8702, 0x743a875d8719]   # request 3 — intact
TRACE send cmd=get_until xtra1=List[0x2f, 0xf]                        # request 4 — CLOBBERED
```

`0x2f`/`0xf` are the small integers `2` and `0`: the cons cell that used to head the `Options`
list now reads `[2|0]`. The service-side trace shows exactly the same `[2|0]`, i.e. **the term
was already corrupt before it was copied**. Consequence: `erl_scan:tokens([], eof, Line, [2|0])`
matches no clause and raises `function_clause` inside the nested (native) run, so
`file_io_server` boxes it as `{error, tokens}`, which `epp:scan_toks/3` (`lib/stdlib/src/epp.erl`
lines 1117-1119) turns into `{error, {4, epp, cannot_parse}}` and an extra `leave_file/2` step.

The same clobbering is what corrupts the `{attribute,2,export,[…]}` form: the epp server holds
the per-form token list in its own state/heap before shipping it as `{ok, Toks}`; when that
region is clobbered, the caller receives a corrupted token list and `erl_parse:parse_form/1`
then yields a non-term. Rendering the token list in the caller (probe 2) shifts the allocation
pattern enough that the clobber lands elsewhere — which is why that probe looked healthy.

## 3. Conclusion

* The native `file_server` (`get_until` → `beam_io::io_device::Continuation` → `erl_scan:tokens/4`)
  is **correct**: it returns the right tokens and the right `eof`, and the same code path from the
  main scheduled process agrees with the oracle.
* The message copier is **correct** for these shapes (plain cross-process control agrees).
* The divergence is that a **scheduled process's heap stops preserving live terms in that
  process's own state** (the epp server's `St#epp.erl_scan_opts` and the in-flight token list)
  after a few request/reply rounds. That points at the scheduled-process heap-management path
  (`beam_rt`'s runner/GC glue driving `Process::grow_nursery` / the nursery chain, and/or
  `beam_process`'s nursery growth), not at `beam_rt`'s file-server, mailbox or CLI layers.

Remaining work: pin the exact nursery-growth/root-publication site and fix it there; then add a
regression test in `tests/compile_file.rs` that drives `epp:parse_file/3` (or `compile:file/2`)
through the scheduled `run` route and asserts the observable parsed form count / produced `.beam`.

## 4. Update — canary instrumentation round (mechanism narrowed, not yet closed)
Raw instrumentation was added in-crate (env-gated `BEAM_RT_TRACE_HEAP`, since removed) that
(a) canaried the live region of every space `Process::grow_nursery` retires and re-read it at
each delivery/resume, and (b) printed the nursery geometry (`base`/`cap`/`used`/`nspaces`) of the
sending and receiving process at those same sites.

### 4.1 Corrections to the earlier record
* **The `BEAM_RT_GC_OFF=1` control is void.** No such env gate exists in the tree (it was a
  temporary gate of an earlier wave and is gone), so that run was the *default* configuration
  and its `NFORMS=6` says nothing about whether a collection is involved. The claim
  "NOT a collection" does not follow from it.
* Control experiments that DO hold: `+hms 233` → `NFORMS=6`; `+hms 500` (and every larger value)
  → `NFORMS=5`; `BEAM_RT_GC_STRATEGY=copying` → 6, `immix` → 5. A small nursery (233 words) is
  what makes the divergence appear, and the repro is deterministic.

### 4.2 Raw trace (the decisive window)
```
[71] RESUME-GEOM pid=3 young(base=A=0x7b54fbc9a790 cap=233 used=233 nspaces=1) mature(0x7b54fbc9aef0 used=0)
[74] PARK       pid=3 young(base=A cap=233 used=233 nspaces=1) mature(0x7b54fbc9aef0 used=0)
[78] RESUME-GEOM pid=3 young(base=A cap=233 used=233 nspaces=1) mature(0x7b54fbc9aef0 used=0)
[81] GROW pid=3 need=2 old(base=A cap=233 used=233) new(base=B=0x7b54fbcc59c0 cap=233 used=0) nspaces=2
[82] GROW pid=3 need=2 old(base=B cap=233 used=233) new(base=C=0x7b54fbcc6120 cap=233 used=0) nspaces=3
[83] PARK       pid=3 young(base=C cap=233 used=230 nspaces=3) mature(0x7b54fbc9aef0 used=0)
[86] DELIVER from=pid2 n=24 to=pid3           <-- Process::deliver -> copy_message refuses -> ensure_capacity
[87] DELIVER-GEOM pid=3 young(base=C cap=233 used=230 nspaces=3) mature(0x7b54fbc9aef0 used=0)
[89] RESUME     pid=3
[90] RESUME-GEOM  pid=3 young(base=C cap=233 used=4 nspaces=1) mature(0x7b54fbccbee0 used=39)
                                                  ^ nspaces 3 -> 1: the chain COLLAPSED (retired A and B freed)
[91] DELIVER from=pid3 n=25 to=pid2 msg={... L[0x7b54fbcc6608][...6 intact elements...]}}   <-- in the CURRENT space C
[93] MUTATED site=deliver tag=retired base=A off=0 old=0x6cf new=0x7b54fbccc640
[94] MUTATED site=deliver tag=retired base=A off=1 old=0x3b  new=0x7b54fbcc59c0   (= B's base)
[95] MUTATED site=deliver tag=retired base=B off=0 old=0x67f new=0x7b54fbc9a790   (= A's base)
[96] MUTATED site=deliver tag=retired base=B off=1 old=0x3b  new=0x0
... further MUTATED lines over both blocks, later showing atom/tuple-header-looking
words (0x80 = arityval(2), 0x2c8b = an ATOM immediates) — i.e. the freed blocks are
being handed out again and filled with live-looking data.
```
Every `alloc_words` growth-failure path writes `[small(2) | NIL]`-shaped words into blocks A and
B through no code of its own: the writes land **after** the collapse in §4.2's window, i.e. in
memory the runtime has already freed.

### 4.3 What this establishes and what is still open
* A **collection runs inside `Process::deliver`** (`beam_process`'s `Process::deliver` →
  `ensure_capacity` → `run_rooted_collection`), not through `beam_rt`'s `collect_or_grow`
  (no `COLLECT-*` log lines were emitted), and it **collapses the nursery chain**, freeing the
  spaces a `grow_nursery` had retired.
* `Nursery::grow` (`beam_gc`) correctly retains the retired space in `Nursery::retired`, and
  `Nursery::spaces()`/`add_to_from_set` do cover every retired space, so the collector *does*
  evacuate a chained nursery. The corruption therefore means a live term of the scheduled
  process is **not reachable from the root set that delivery-driven collection gathers** — the
  collector then does not evacuate it, `collapse` frees its block, and the writer that re-uses
  the block produces the observed `[2|[]]`-style clobber.
* The delivery-driven collection (`Process::deliver`) is the one collection site that does
  **not** consult the unscanned-root valve that `beam_rt`'s `ensure_heap` / `ensure_heap_full` /
  `alloc_words_rooted` apply (`context::gc::root_is_unscanned`) — worth checking first, together
  with which of the parked machine's regions `run_rooted_collection` actually gathers.
* The canary's `MUTATED` output alone cannot separate "written while still retired-and-live"
  from "written after the block was freed"; the round that was in flight when the turn budget ran
  out replaced it with a per-pid block registry (live vs freed) plus a scan of the published
  roots (`x`/stack/`extra_roots`) for pointers into a **freed** block. That scan is the piece that
  pins the exact dangling root and must be the first thing a follow-up wave re-adds.

### 4.4 Repro / controls (authoritative, cheap)
```
# bug (scheduled route):
erts_rust/target/debug/beam_rt -noshell run <dir>/b13.beam b13:go/0        # NFORMS=6
# correct (single-pass route):
erts_rust/target/debug/beam_rt -noshell -pa <dir> -eval 'b13:go()'        # NFORMS=5
erl -noshell -pa <dir> -eval 'b13:go()' -s init stop                      # NFORMS=5 (oracle)
# nursery-size control:
erts_rust/target/debug/beam_rt -noshell +hms 500 run <dir>/b13.beam b13:go/0   # NFORMS=5 (masked)
```
`b13.erl`'s `go/0` hard-codes an absolute path; a committed regression test must not.

## Round-3 freed-block scan: NO dangling published root (decisive negative)

A temporary `BEAM_RT_TRACE_ROOTS` instrument (since removed) registered every nursery block base a
pid had ever been observed using, marked a block **voided** once `AnyCollector::young_contains(base)`
turned false (the delivery-collapse of the nursery chain), and at every runner observation
(`entry`, `post-resync`, `park-yield`, `park-wait`) scanned **every** holder word —
`Process::registers()` (`regs.x` whole file, whole stack, `extra_roots`), the process dictionary,
`saved_args`, `signals`, every mailbox `payload`/`from`/`token`, and the parked `Machine`'s
`x`/stack/`fr`/`pending_exception` — reporting every LIST/BOXED word whose target address landed
inside a voided block.

Raw output (`b13:go/0` on the scheduled `run` route, default `+hms`):

```
ROOTS-GEOM pid=3 site=park-wait   nspaces=3 cur=0x7bbd53f66120(cap 233 used 230) freed=0
ROOTS-GEOM pid=3 site=entry       nspaces=1 cur=0x7bbd53f66120(cap 233 used   4) freed=1
ROOTS-GEOM pid=3 site=post-resync nspaces=1 cur=0x7bbd53f66120(cap 233 used   4) freed=1
... (repeated across the whole run; freed stays 1)
NFORMS=6
```

**Zero `ROOTS-DANGLING` lines** — the voided block received no holder word from any published root
or machine root at any observation. Earlier (unfiltered) rounds only ever reported `m.x[i]` at the
`entry` site, i.e. the **parked machine's own stale register file**, which the resume replaces
wholesale: `beam_rt/src/context/gc.rs::scatter_roots_full` requires
`regs.x().live_slice().len() == roots.x.len().min(MAX_REG)` and `publish_roots_full` sets
`regs.x_mut().set_live(roots.x.len().min(MAX_REG))` (= the whole register file) before copying
`roots.x[..live]`, so the post-resync scan is clean too.

### Current invariant conclusion
`beam_rt`'s park/resume round-trip publishes and restores the **whole** engine root set
(`RootSet::ALL_LIVE` x + stack + FR, plus `pending_exception` via `extra_roots`), and
`beam_process::Process::run_rooted_collection` gathers exactly those families. No published-root
omission is observable: the surviving term is **not** reached through a voided block at all, so the
earlier "live term left in the retired space" reading does not survive this range-based re-scan.
The remaining candidate is a term clobbered **in place** in the still-current block (`used` cycles
233 -> ~4 on the *same* base `...f66120`, i.e. the nursery is reused without alternating blocks) —
inside the collector (`beam_gc`), not in `beam_rt`'s published root set.
