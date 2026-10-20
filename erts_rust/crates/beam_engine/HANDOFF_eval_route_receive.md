# HANDOFF — the two `-eval`-route `receive` gaps (host half lives in `beam_rt`)

Status: the **engine half is done** (this crate, `FunOutcome` / `apply_fun_outcome`, see
`CONTEXT.md` -> *The closure-apply suspend channel*). Both user-visible gaps remain **open** and
both are **host-side** (`crates/beam_rt`, a sibling node of this one). This note is the
ready-to-apply record: root cause with `file:line`, the exact patch, the oracle transcripts and the
verification commands. Line numbers were read at commit `2a16d197f1` and drift; the quoted code is
the anchor.

Two probes, run against the system oracle (`erl`, OTP 29.1.1) and the Rust binary
(`./erts_rust/target/debug/beam_rt`):

```sh
B=./erts_rust/target/debug/beam_rt
# Gap 1 — a spawned `-eval` closure cannot block
$B -noshell -eval 'P = spawn(fun() -> receive M -> M end end), timer:sleep(150),
                   io:format("alive=~p~n",[is_process_alive(P)]), halt(0).'
#   rust:   alive=false          oracle: alive=true
$B -noshell -eval '{_,R} = spawn_monitor(fun() -> receive M -> M end end),
                   receive {'"'"'DOWN'"'"',R,_,_,W} -> io:format("~p~n",[W]) end, halt(0).'
#   rust:   'unsupported_opcode: receive (no after clause) inside a spawned process'
#   oracle: (child is alive; no DOWN)
# Gap 2 — `receive … after T` fires late
$B -noshell -eval 'T0 = erlang:monotonic_time(millisecond),
                   R = receive late -> {msg,erlang:monotonic_time(millisecond)-T0}
                       after 300 -> {timeout,erlang:monotonic_time(millisecond)-T0} end,
                   io:format("~p~n",[R]), halt(0).'
#   rust:   {timeout,351}        oracle: {timeout,301}
```

The same `receive` with an `after T` clause is *semantically* right on this route (a spawned
closure's `after 2000` really waits 2000 ms); only the timing (Gap 2) and the unbounded arm
(Gap 1) are wrong.

---

## Gap 2 — `receive … after T` is late by up to a whole pump call. FIXABLE ENTIRELY IN `beam_rt`.

### Measured

| probe (`T0 = monotonic_time(ms)` first) | rust | oracle |
|---|---|---|
| `receive … after 1` / `25` / `50` / `100`, nothing arrives | **150 / 150 / 151 / 151** | 1 / 25 / 50 / 100 |
| `receive … after 300` | **352** | 301 |
| `receive … after 1000` | **1132** | 1001 |
| `spawn(fun() -> P ! late end), receive late … after 300` | **{msg,126}** | {msg,0} |
| `send_after(100,self(),late), receive late … after 300` | **{msg,150}** | {msg,101} |
| two sequential receives, `after 150` then `after 300` | **{timeout,505}** | {timeout,452} |
| controls: `timer:sleep(300)` at top level / a compiled `spawn(timer,sleep,[200])` | 302 / parks correctly | same |

The flat `after 1 … after 100` ⇒ **exactly 150 ms** is the smoking gun: a **fixed cost** is paid
before the deadline is ever looked at. The controls prove the defect is specific to the `-eval`
`receive` loop — the compiled route's finite-receive deadline path (`pump_once`,
`DEADLINE_PUMP_SLEEP = 5 ms`) is already ±2 ms.

### Root cause

`crates/beam_rt/src/eval/mod.rs:1521` `Evaluator::receive`, the `Some((timeout, body))` arm:

```text
:1528  loop {
:1529      try_mailbox_receive(..)                       // scan
:1536      if may_pump_scheduler() {
:1537          let ran = runtime.pump_scheduler(RECEIVE_PUMP_SLICES);  // *** the unit of latency
:1539          if ran > 0 { continue; }
:1542      let remaining = deadline - now;
:1543      if remaining.is_zero() { break; }              // the ONLY deadline test
:1552      let slice = remaining.min(PORT_POLL_SLICE);    // 25 ms
:1564      poll_port_messages(slice)
:1584      if !may_pump_scheduler() { break; }
:1593      let wake = deadline.min(now + PORT_POLL_SLICE);
:1594      runtime.wait_for_timer(wake, RECEIVE_PUMP_SLICES);
:1597      if now >= deadline { break; }                  // breaks WITHOUT rescanning
:1600  }
```

`Runtime::pump_scheduler` (`crates/beam_rt/src/runtime/scheduler.rs:242-246`) is
`scheduler_pass(Some(max), true, TimerWait::AtMost(TIMER_PUMP_SLICE = 100 ms))`
(`scheduler.rs:74`). Two sleeps live inside that one call and neither is a function of the
caller's window:

* **timer half** — `timer_wait(AtMost(100 ms))` returns `remaining.min(100 ms)` whenever *any*
  deadline is pending, and the pass sleeps it (`scheduler.rs:1125-1136`, arm `:1493`). A booted
  kernel keeps housekeeping deadlines, so a **zero-slice** round costs 100 ms that has nothing to
  do with this `receive`.
* **port half** — `port_timeout` is `PORT_POLL_SLICE` for every zero-slice round
  (`scheduler.rs:905-911`), i.e. `epoll_wait(…, 25 ms)`.

`TimerWait::allows` (`scheduler.rs:147-155`) permits one sleep per pass, so an empty run queue
costs **25 + 100 + 25 = 150 ms** per pump call. The mailbox and the deadline are only re-checked
*between* whole pump calls, so the overshoot is one pump call plus up to two 25 ms slices. The
method's own docs (`eval/mod.rs:1493-1520`) claim "the window, not the slice, is what ends the
wait, so the observable timing is unchanged" — **false**; that paragraph must be rewritten with the
fix.

### The patch (3 places, no cross-crate change)

1. `crates/beam_rt/src/runtime/scheduler.rs` — a new bounded, **non-blocking** pump entry beside
   `pump_scheduler` (`:242`):

```rust
/// The `-eval` `receive … after T` pump: a **bounded, non-blocking** pass.
///
/// Same work as [`Runtime::pump_scheduler`] (run queue, port deliveries, distribution
/// pumps, due timers — all non-blocking by construction) but under
/// [`TimerWait::ImminentWithin`] with a **zero** window, so the pass never sleeps on a
/// timer deadline and hands `deliver_port_messages` a zero poll whenever no port owes a
/// reply. The caller does its own blocking port poll (`poll_port_messages`).
pub(crate) fn pump_scheduler_now(&mut self, max_slices: u64) -> Result<u64> {
    let (ran, delivered) = self
        .scheduler_pass(Some(max_slices), true, TimerWait::ImminentWithin(Duration::ZERO))?;
    Ok(ran + delivered as u64)
}
```

   `TimerWait::ImminentWithin` already zeroes the port half when no reply is owed
   (`scheduler.rs:905-911`) and a zero window can never sleep (`scheduler.rs:1500`), so this is
   one line of new policy, no new machinery. It cannot spin: `allows(waits)` is `waits == 0`.

2. `crates/beam_rt/src/runtime/scheduler.rs:320-334` — make `wait_for_timer` honour the bound its
   own docs promise ("cannot outlive the caller's deadline"): replace the
   `pump_scheduler_boot(max_slices)` tail with a pass under
   `TimerWait::ImminentWithin(until.saturating_duration_since(Instant::now()))`. `pump_scheduler_boot`
   itself stays as-is (the boot drives need its 100 ms window).

3. `crates/beam_rt/src/eval/mod.rs:1528-1600` — three surgical changes:
   * **hoist the deadline test above the pump** (insert after the scan at `:1532`, drop the now
     duplicate `:1543-1545`): a closed window pays no pump, no poll and no sleep — and because the
     scan ran first, a message that arrived at the boundary still wins (the oracle's
     `send_after(100) … after 300` answers `{msg,101}`, never `timeout`);
   * `:1537` `pump_scheduler` -> `pump_scheduler_now` (comment: the `AtMost(100 ms)` sleep inside
     `pump_scheduler` is what made `after T` overshoot by up to a whole pump call);
   * **delete `:1597-1599`** so the loop head rescans the mailbox after the final
     `wait_for_timer` pass instead of handing a just-delivered message to the `after` body.

   Keep, unchanged: `slice = remaining.min(PORT_POLL_SLICE)`, the `RECEIVE_WAIT_ITERATIONS` bump,
   `poll_port_messages(slice)`, the non-pumping `break` at `:1584-1586`, `wake` /
   `wait_for_timer`. The **bare-receive** arm (`:1604-1624`) keeps the *blocking* `pump_scheduler`
   at `:1618` — there the sleep is exactly what lets a runnable child deliver, and there is no
   deadline to be late for.

   Contract A.5 is **strengthened**, not broken: re-entering the pump every iteration (<= ~25 ms)
   instead of every ~200 ms only gives the distribution pump more passes, and every
   `pump_dist*`/`bind_dist_peers`/`dispatch_dist_*_inbound` half is non-blocking.

### Expected after the patch

`after 1/25/50/100` -> 1-2 / 25-26 / 50-51 / 100-101; `after 300` -> 300-301;
`after 1000` -> 1000-1001; immediate child send -> 0-2; `send_after(100)` -> 100-102; the two
sequential receives -> ~452. Residual by construction: message pickup stays bounded by one 25 ms
poll slice.

Docs the patcher owes: `eval/mod.rs:1493-1520` (the "timing unchanged" paragraph) and
`crates/beam_rt/src/runtime/CONTEXT.md:18` ("`pump_scheduler` keeps `TimerWait::AtMost` for the
`-eval` receive pump, whose bounded-100 ms semantics its `receive` loop relies on"). Existing pins
to re-check: `eval::tests::a_long_after_window_is_waited_out_in_port_poll_slices` (`>= 2`, `<= 100`
iterations for 100 ms — ~4 after the fix) and `a_sub_slice_after_window_never_loops`
(`<= 1` for 5 ms).

---

## Gap 1 — a spawned `-eval` closure cannot block. NOT fixable in `beam_engine`; two sound host routes.

### Path and root cause

1. `spawn/1` on an `-eval` closure creates `ProcessEntry::Fun { fun, target: None }`
   (`crates/beam_rt/src/runner/entry.rs:47-88`, spawned at `:443`).
2. `RunnerState`'s first run asks `local_fun_target`, which deliberately answers `Ok(None)` for
   `uniq == EVAL_FUN_UNIQ` (`crates/beam_rt/src/context/apply.rs:126-129`, sentinel at
   `eval/mod.rs:125`), so the process gets **no machine entry** at all.
3. Every slice then runs `budget_ctx.apply_fun(*fun, &engine.args)`
   (`crates/beam_rt/src/runner/run.rs:1099-1111`) -> `RuntimeContext::apply_fun`
   (`context/execution.rs:590-606`) -> `apply_non_local_fun` (`context/apply.rs:201-231`) ->
   `crate::eval::apply_eval_closure` (`eval/mod.rs:501-525`), which builds a **fresh `Evaluator`**
   over the `Rc<FunBody>` and returns `Result<Eterm>`.
4. `Evaluator` (`eval/mod.rs:528-545`) is a **recursive-descent tree walker**: its continuation is
   the native Rust stack. There is no serialisable continuation, no `(func, pc)` to report, and
   nothing for `Machine::resume_outcome` to resume. So a bare `receive` is refused
   (`eval/mod.rs:1613-1617`) and that refusal terminates the **process**.

The sibling shape dies one level deeper: a `timer:sleep(50)` inside such a closure reaches the
loaded `timer:sleep/1` (itself `receive after T -> ok end`) through
`RuntimeContext::run_export_blocking` (`context/pump.rs:259-296`), whose `Machine` is a
**stack local** (`:280`) and whose `pump_once` (`:540`) refuses on its first line
(`:547-549`, `if !self.may_pump_scheduler() { return Ok(None) }`) — folding the callee's
`RunOutcome::Wait` into `beam_engine::blocked()`. The child then dies with
`'unsupported_opcode: run: process blocked (no message available)'` where the oracle sends its
message. That refusal is a **soundness invariant, not policy**: `scheduler_pass` moves the process
table out for its duration (`let mut table = self.take_processes()`, `scheduler.rs:823`), so a
re-entrant `pump_scheduler` from inside a slice would run an empty table.

Control proving it is the closure, not the spawning: `spawn(timer, sleep, [200])` — a **compiled**
export — parks and resumes correctly on its own machine.

### Why the engine cannot close it

* On this route **no engine machine is running**: the runner calls the host's own `apply_fun`
  trait method, so there is no instruction to park on and `RunOutcome` has nothing to name.
* `RunOutcome::{Wait, Suspend}` are *defined* as an address into
  `Module::functions[func].code[pc]` (`src/machine/run.rs:55-102`, `src/machine/drive.rs:181-204`);
  an AST walker has no such address.
* `BifOutcome::Suspend` — and now its closure twin `FunOutcome::Suspend` — are legal only where
  resumption **re-executes** the instruction (`src/context.rs`: the "parking is `(func, pc)` plus
  the root set, resumption re-executes" contract); a walker that already applied side effects
  before its park point cannot be re-executed. That is the class-A/class-B rule this crate already
  encodes (`families/funs/mod.rs`, `apply_target` stays class B).

### The two sound host routes (both `beam_rt`, both large — not a patch)

* **(A) A resumable evaluator.** Give the walker an explicit continuation
  (`Flow::Suspend { cont }` over a heap `enum Cont { Expr(..), Exprs(..), .. }`), a store beside
  `Runtime::eval_closures`, a `ProcessEntry::EvalClosure { cont, env }` arm the runner parks and
  re-enters, plus a host-side park for the nested `Machine` (`RunnerState` would hold
  `(Cont, Machine)`).
* **(B) Compile the closure into real code.** `beam_rt` already compiles in-process (contract A.9,
  755/755 sources byte-identical): emit the closure as a synthetic module export whose parameters
  are the captured values and make the spawned entry a normal `ProcessEntry::Export` /
  `ProcessEntry::Fun { target: Some(..) }`. Then a bare `receive` *and* a nested `timer:sleep`
  park the process for free on the existing `run_label_outcome_with_tier` route. The parser/AST
  for (B)'s emitter already exist (`eval/{parse,ast}.rs`); cost is compile latency at spawn plus
  free-variable marshalling.

Anti-patterns to reject: (a) leaving the process alive-but-permanently-parked (the message is
never consumed and a later delivery cannot resume it — silent wrong behaviour, not a typed gap);
(b) re-running the closure body when a message arrives (replays every earlier side effect, and
re-reads the clock/`make_ref`).

---

## The engine half shipped here

`FunOutcome` + `ExecutionContext::apply_fun_outcome` (`src/context.rs`), wired at the single
apply fallback (`src/families/funs/mod.rs`, the `Ok(None)` arm of `resolve_local_fun`), so a host
that **interprets** a closure can now answer "I must wait" and have the engine park the machine on
the `call_fun`/`call_fun2` instruction (`RunOutcome::Suspend`, resumed by re-consulting the door
with the same `fun`/`args`; the host must CONTINUE, never restart, the pending application).

It does **not** close Gap 1 (no machine runs on that route). It removes the last class-B wall for
the shapes where the engine *is* driving — e.g. `lists:map(fun(X) -> receive … end, L)` once the
host has a resumable evaluator (route A) — and it is the prerequisite for route A's in-machine
half. Gate: `tests/fun_suspend.rs` (6 cases, both tiers).

## Verification

```sh
erts_rust/scripts/cargo.sh test  --manifest-path erts_rust/Cargo.toml -p beam_engine
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --bin beam_rt
# then the two probes at the top of this file, plus
erts_rust/tools/differential/gate.sh          # after the beam_rt patch
```
