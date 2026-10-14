# `beam_rt` — Observability & Performance Detail

Companion to [`CONTEXT.md`](CONTEXT.md). It holds the current-state detail for the observability
modules (`src/trace.rs`, `src/seq_trace.rs`, `src/crash_dump.rs`, `src/perf_map.rs`), the
execution-trace seam, and the per-call performance-measurement harnesses — split out so `CONTEXT.md`
stays under the Context Tree per-file read limit. It is documentation of the present state, not a
change log.

## Observability modules (contract A.8)
* **`trace.rs`** — ordinary process tracing: `TraceFlags`/`TraceEntry`/`TraceState` (the per-pid flag + tracer table) and the `PatternTable` of `{M,F,A}` `trace_pattern` entries; the BIF half is `erlang:trace_info/2` (`flags`/`tracer`/`match_spec`/`traced`/`all`), `trace_delivered/1`, `erts_internal:trace/3` and `trace_pattern/3`. `RuntimeContext::emit_trace_event` is the single emitter (tracer lookup, `silent` flag, the discard rule, the `trace_ts` flavour, delivery through the internal bypass).
* **`seq_trace.rs`** — sequential tracing: the per-process `Token` (`{Flags, Label, Serial, From, Lastcnt}`), the node-global serial clock, the `FLAGS`/`flag_bit`/`decode_flags`/`encode_flags` bitmask, the four `erlang:seq_trace*/seq_trace_print*` BIFs and the `reset_seq_trace` arm of `system_flag/2`; `seq_trace_message`/`seq_trace_send_message` build the `{seq_trace, Label, …}` events (no system tracer is ever configured, so `SeqTrace::tracer` stays `None`).
* **`crash_dump.rs`** — the ERTS-format crash-dump writer behind `erlang:halt`: `snapshot(&Runtime, slogan, seconds) -> DumpInput` + `write_dump(path, dump)` reproduce the oracle's section vocabulary and order (`=erl_crash_dump:0.5` … `=end`); `crash_dump_path(&ErlEnv)` reads `ERL_CRASH_DUMP`/`ERL_CRASH_DUMP_SECONDS`. Two safe-Rust substitutions: `=proc_stack` renders the engine X registers, `=proc_heap` the reachable boxed terms.
* **`perf_map.rs`** — the `+JPperf`-gated JIT symbol map: `PerfFlag` parses the C's accumulating value grammar, `PerfMapConfig` honours `+JPperfdirectory`/`ERL_SYM_MAP_FILE` (default `/tmp`), `PerfMap::new` creates `perf-<pid>.map`/`jit-<pid>.dump` at construction. **No range is ever fabricated** — every line is a real cached range: when `+JPperf` is enabled the runner feeds `beam_engine::jit::cache::baseline_code_map` for the process's module into `PerfMap::record` (the single hook); with `+JPperf` off nothing is recorded and the map is well-formed but empty. A map/compile error is routed to `PerfMap::note_diagnostic`, never into the run.

**Emitted vs deferred trace events.** *Emitted*: `send`, `'receive'` (on **enqueue**, gated on the receiver's flag), the sequential-trace `{seq_trace, Label, {send, …}}` event (the sender's token carries `FLAG_SEND`), and the whole **execution family** — `call`, `return_from`, `return_to` — produced by `src/context/trace.rs`. *Deferred* (unemitted rather than an unpinned shape): `spawn`, `exit`, `register`/`unregister`, `in`/`out`; `PatternTable::matches` is the predicate such a hook calls.

## The execution-trace seam (`src/context/trace.rs`, contract A.8)
`beam_engine` already reports every call/return it executes — `ExecutionContext::trace_enabled` (latched once per run into `Machine::trace_on`) then `ExecutionContext::trace_event(TraceEvent::{Call,ReturnFrom,ReturnTo})` — and this crate is its one consumer: the two trait methods live in `src/context/mod.rs` (Rust allows one impl block per trait per type) and delegate to `RuntimeContext::{execution_trace_enabled, execution_trace_event}` in `src/context/trace.rs`. That module decides *matching* (the installed `erlang:trace_pattern/3` table) and builds + delivers each event through the single emit point `emit_trace_event`. Semantics are the oracle's: the process `call` flag gates the whole family (`[return_to]` alone reports nothing); `local`/`global` are mutually exclusive pattern flags and a `[local]` pattern fires for every entry (a `[global]` one only for an external dispatch); `return_from` needs a `{return_trace}`/`{exception_trace}` match-spec **action** in the match function's body (`MatchAll`/a bare atom arm nothing); `return_to` is armed at the **traced local call** and reported when the callee returns, naming the caller (which needs no pattern of its own — the arm is threaded between the engine's paired `ReturnFrom`/`ReturnTo` through a `thread_local!` slot, legitimate because `Runtime` is `!Send`); the `arity` flag swaps the `call` event's third element from the argument list to the bare arity; a pattern-level `silent` suppresses the event on top of the process `silent` flag. The seam is live on **both** execution paths: the single-pass `Machine::run*`/`Runtime::run_function`/`run_module` entry *and* a **scheduled** process, because `src/runner/budget.rs`'s `BudgetContext` forwards `trace_enabled`/`trace_event` to the slice's real `RuntimeContext` like every other method. A scheduled process therefore reports its `call`/`return_from`/`return_to` events for every call/return instruction its tier helper-executes (under the default `JitMode::Auto` both its first entry *and* every later slice — a pre-emption or a `receive` resume — run on the baseline tier, chosen once at the first entry and remembered for the process's whole life; `BEAM_RT_TIER=interpreter` is the kill switch that pins the interpreter) — an instruction a native tier **inlines** (a linked local tail call) is not reported, the documented tier limitation, never a wrong result. Both paths are covered end to end by `tests/trace_call.rs`.

**Documented divergence.** `TraceEvent::Call` does not carry the call kind, so "an external dispatch" is approximated as `callee_module != current_module` (`current_module` is the module whose code is executing; `None`, the `-eval` evaluator, counts as "differs"). A `[global]`-only pattern matching a call written `?MODULE:F(...)` — a `call_ext` whose callee module *is* the current module — is therefore **under-reported**; the whole `[local]` surface, every cross-module `[global]` entry and the no-pattern case agree. Shapes and semantics are cross-checked against the live OTP 29.1.1/erts-17.1 oracle (the `silent` pattern flag is OTP-30-only and not locally probe-able).

**The `tracer == tracee` discard rule.** `emit_trace_event` drops an event whose tracer **is** the tracee (mirroring `erl_tracer_nif.c:143-146`); a `silent` tracee and a tracer no longer in the process table are dropped the same way, never as an error.

## Performance measurement

### Per-call overhead regression harnesses
**`examples/perf_bif.rs`** (run `erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_bif`): compiles `pc:add/2`/`pc:loop/2` with `erlc` and reports the marginal ns/call as a **two-point slope** `(t(N2)-t(N1))/(N2-N1)` (default N1=16M/N2=64M, best-of-5 + a warm-up per point), cancelling process start-up exactly; a non-positive slope auto-scales then fails loudly rather than reporting noise.
`N-1`/`N+1` are `gc_bif` **instructions** the baseline tier inlines natively, so the figure is the native-loop cost — [`PERF.md`](PERF.md)'s `perf_bif` table records `pc:add` at **4.39** and `pc:loop` at **5.34** ns/call, and that file reads the pair as ~4 ns/iteration — NOT a BIF round trip.
**This fixture cannot measure the registered-BIF path at all** (a BIF it does not inline, such as `trunc/1`, needs the recipes in the dispatch-cost section below), and PERF.md records its marginal-op column (`0.53` vs `0.08` ns) as below one cycle and therefore unresolvable at that resolution, so the row figures, not the marginal, are what to quote.
`$PERF_BASELINE_ADD_NS`/`$PERF_BASELINE_LOOP_NS` add a delta/speedup column.

**`examples/perf_call.rs`** (run `erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_call [ITERATIONS]`; default **100 000** calls per sample, one warm-up, best of 5): the **in-process** registered-BIF host-call micro-benchmark — no CLI subprocess, no `-eval` parser and **no engine** — holding a real `RuntimeContext` and calling `beam_engine::ExecutionContext::call_bif` in a loop, so what is on the clock is the dispatch prologue, the registered `beam_bifs` body, the result copy-out and the pooled-heap rewind.
Every row names the path it measures: **A** `RuntimeContext::new` (the `-eval`/single-pass shape — `mailbox_table` is `None`, so the copy-out is skipped by construction and the pooled heap is not rewound), **B** `RuntimeContext::for_process` (the scheduler shape — copy-out plus pooled rewind ON) and **C** `Runtime::run_scheduled` (a compiled `.beam` as a scheduled process, two-point slope so the per-run spawn cancels), with a `control` row (the same loop with inlined arithmetic only) as the floor every marginal is taken against.
Its own smoke test [`tests/perf_call.rs`](tests/perf_call.rs) drives the built example binary and pins the printed row/block set, and a missing `erlc` (path C compiles its fixture with it) is a **loud skip that exits 0**, never a silent pass.
The measured per-call figures live in [`PERF.md`](PERF.md)'s `## DISPATCH-CUT` and `## The BIF-result copy-out cut (COPY-OUT-CUT)` sections.

### Dispatch cost (registered-BIF path)
The registered-BIF path (`call_registered`) runs per call, so any handle built there must be gated by a `uses_*` predicate — a snapshot that walks the code server / process table / ETS registry, or clones the caller module, is a per-call cost.
Both the per-key prologue (the registry lookup plus the whole `uses_*` roster) and the resolved `BifFn` are answered from the per-context [`dispatch` memo](src/context/CONTEXT.md#the-dispatch-memo--dispatchhooks) on every call after the first, so they are paid once per key per run and the registered body reaches `beam_bifs` without a second `BifRegistry` lookup.

[`PERF.md`](PERF.md) is the authority for every per-call figure; none is restated here as a fresh measurement.
Its `## DISPATCH-CUT` section holds the dispatch-prologue cut measured by `examples/perf_call.rs`: the immediate-returning path-A rows at **0.69–0.75×** their `BEFORE` values (`erlang:abs/1` **46.51** ns/call, `erlang:trunc/1` **53.81**, `lists:reverse/1` **65.42**, `erlang:'++'/2` **53.94**, `lists:seq/2` **72.29**), the ETS rows at **0.90–0.92×** (`ets:insert/2` **196.26**, `ets:lookup/2` **149.70**), and the copy-out-bearing path-B container rows at **0.88–0.89×** (`lists:reverse/1` **126.76**, `lists:seq/2` **192.87**), against a **0.11** ns control.
Its `## The BIF-result copy-out cut (COPY-OUT-CUT)` section holds that cut's interleaved path-B binary A/B: `lists:seq/2` **166.65** ns/call at **0.877×** its `BASE`, `lists:reverse/1` **112.02** at **0.950×**, and `ets:lookup/2` **180.94** at **0.975×** — noise-dominated and explicitly **not claimed as a win** — against a **0.15** ns control, with the permanent- and exhaustion-refusal behaviour pinned by `src/context/calls.rs`'s own unit pair.
Its `Registered-BIF (non-inlinable) per-call` sections are the authority for the CLI-level `nb*` fixtures (`nb:bif/2` = `trunc/1` and `nb2:absb/2` = `abs/1`): `BEFORE` column **302.1** / **322.3** ns/call, `AFTER` column **85.52** / **96.13** ns/call, marginal **≈ 79** ns.

Measuring the **registered-BIF** path needs a loop body carrying a BIF the baseline tier does **not** inline (so the `gc_bif`/`call_ext` really reaches `call_bif` → the registry), driven through the runtime's own `run` action, with the two loop points **interleaved** — the host is shared, so only interleaved minima are comparable.
Recorded on that method: **≈ 283 → ≈ 76 ns/iter** for `pb:tr/2`'s `trunc/1` across the dispatch-memo cut, the cross-crate note [`PERF.md`](PERF.md) cites alongside the `nb*` columns above.

Reproduce the `pb:tr/2` figure:

```sh
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p beam_rt
mkdir -p "$TMPDIR/pbrepro"
printf '%s\n' '-module(pb).' '-export([tr/2]).' 'tr(0, Acc) -> Acc;' \
  'tr(N, Acc) -> tr(N-1, Acc + trunc(N)).' > "$TMPDIR/pbrepro/pb.erl"
erlc -o "$TMPDIR/pbrepro" "$TMPDIR/pbrepro/pb.erl"
# two interleaved points, min of N per point, slope (t(N2)-t(N1))/(N2-N1):
for i in 1 2 3; do
  for N in 16000000 64000000; do
    start=$(date +%s%N)
    erts_rust/target/release/beam_rt run "$TMPDIR/pbrepro/pb.beam" pb:tr/2 "$N" 0 > /dev/null
    echo "N=$N $(( ($(date +%s%N) - start) / 1000000 )) ms"
  done
done
```

`trunc/1` compiles to a `gc_bif` the baseline tier leaves to the interpreter, so each iteration is a real `call_bif` → `call_registered` round trip; the `-`/`+` around it are inlined and cancel in the slope.
