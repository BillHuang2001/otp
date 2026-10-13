# A.9 scheduled (`run`) route: the monitor-DOWN signal growth that would relocate a woken watcher's heap

Sibling-owned dossier for the A.9 **scheduled (`run`) route** — the two differential corpus cases
`selfhost/in_process_compile_file_no_spawn_deterministic` (E0:
`compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}])`) and
`selfhost/scheduled_process_compile_file_deterministic` (the default shape through `compile:file/2`'s
own compiler process). Both **agree** with the oracle on both execution tiers; the corpus is
`total=1868 agree=1866 mismatch=0 unsupported=2`. Present state, not a history. The current picture:
the `beam_process` signal-driven collection site that would relocate a woken monitor watcher's heap
against an incomplete root set is **declined by the host collection gate** the `ptab.rs` signal sites
consult (`beam_rt`'s `RuntimeCollectionGate`), with the runner's park-window relocation exemption as
the complementary mitigation.

## 1. The pinned mover — the monitor-DOWN branch of the process-termination cascade
A heap-move trace that watches the **mature base** (not only the nursery) pins the move to the
process-termination cascade's monitor step, entirely inside `crates/beam_process/src/ptab.rs`:

```
beam_process::Scheduler::run_ready
  → slice_end
  → ProcessTable::terminate_with_ports      (ptab.rs:1778)
  → terminate_and_cascade                   (ptab.rs:1791)
  → terminate_into                          (ptab.rs:1840, monitor step 2)
  → enqueue_down_signal                     (ptab.rs:1932)
  → copy_reason_onto                        (ptab.rs:301)
  → the copy_into_heap exhaustion arm
  → grow_receiver_for_signal                (ptab.rs:170)
  → Process::ensure_capacity(need_words)    (ptab.rs:183)   ← a rooted collection
```

The collection would relocate the **monitor watcher's** heap. The watcher was just woken
`Waiting` → `Runnable` by a preceding delivery, so
`Process::has_unpublished_roots()` (= `relocation_exempt || state == ProcessState::Running`,
`beam_process/src/process.rs:1098`) is `false` — the crate-owned predicate alone passes — even though a
woken-but-not-yet-resumed process's `regs` snapshot is **not** its whole live root set (the parked/
woken machine's live terms are not all published). The signal site therefore consults the **host**
`CollectionGate` beside that crate predicate (`ptab.rs`'s private `declines_signal_collection`, which
OR-s the two), and `RuntimeCollectionGate` declines the relocation for a receiver whose gathered roots
reach a container in an unscanned region, so the signal takes the heap-fragment route the cascade
already uses for a `Running`/exempt receiver. Measured on the trace that pinned the mover: mature base
`0x70876d95dc10` → `0x70875d099090`, nursery used 232 → 20 words.

## 2. Ruled out by measurement
* The valved **delivery receiver-collect**: forcing `RuntimeContext::delivery_collection_is_unsound`
  (`src/context/mod.rs`) to `true` for **every** receiver changed nothing.
* The `beam_rt` admission doors and the park arm: `Runtime::serve_gc_request` (`src/runtime/gc.rs`)
  and `context/gc.rs::collect_or_grow`/`alloc_words_rooted` never fired.
* Any `beam_rt`-level delivery: `deliver_into_mailbox` PRE/POST the receiver heap was unchanged —
  every `beam_rt` pass-boundary delivery already exempts the receiver, because
  `RuntimeContext::for_process` sets `mailbox_pid = pid`.

## 3. The mitigation (in place)
**The host collection gate.** `Runtime::new` installs `RuntimeCollectionGate` (`src/context/gc_walk.rs`)
on the runtime's process table, and every `ptab.rs` signal site (`grow_receiver_for_signal`,
`build_signal_message`, `copy_reason_onto`, `deliver_head_signal`, `enqueue_exit_signal` /
`enqueue_down_signal`) consults it (OR-ed with the crate's `has_unpublished_roots`) before a
*collecting* `ensure_capacity`, so a foreign-reachable receiver takes the no-relocation
`Process::grow_nursery` / heap-fragment route instead. The gate owns its own `WalkScratch` (a signal
site runs from a different borrow than an admission point and may even run mid-collection, so sharing
the runtime's scratch would alias) and always descends (`descend = true` — a signal site has no
in-flight machine to publish and cannot evaluate the per-pid descent latch), so it decides exactly as
the runtime's admission-point valve does.
**The runner's Wait park arm** (`src/runner/run.rs`) additionally marks the process
**relocation-exempt** for the whole parked window, and the **resume** clears it. The two are
**complementary**: the gate declines only when an unscanned container is reachable from the receiver's
**gathered** roots, whereas the exemption refuses *any* relocation for the parked window — also
covering a live term the host holds **outside** that published set (the runner's own entry terms —
`ProcessEngine::args`, `ProcessEntry::Fun::fun`, `LocalFunTarget::args` — and any live `Eterm` in host
Rust state), which the walk cannot see. The mitigation is **decline-only and bounded**: it never
*allows* a collection, so an affected process's heap grows on that path only until it is resumed, and
its own admission point then collects as usual.
The regression is `tests/compile_file.rs::a_scheduled_process_compiles_a_source_file_in_process_like_the_oracle`
— the E0 in-process `compile:file/2` driven through the scheduled `run` route over the committed
`tests/fixtures/sched_compile_{driver,input}.erl` — which fails without the mitigation
(`invalid boxed header subtag`; `jit_step_one` ← `jit_ext_call`) and passes with it on both tiers;
`a_scheduled_process_parses_a_source_file_like_the_oracle` covers the same class via `epp:parse_file/3`.
The seam is pinned by
`context::tests::the_installed_gate_declines_a_signal_driven_relocation_of_a_foreign_reachable_receiver`
and `runner::tests::a_parked_process_stays_relocation_exempt_for_its_parked_window`.

## 4. Where the refusal lives
`crates/beam_process/src/ptab.rs` owns the seam: `ProcessTable::set_collection_gate` /
`take_collection_gate` / `has_collection_gate` install/take/test the host `CollectionGate`, and its
private `declines_signal_collection(pid)` OR-s that gate with the crate's `has_unpublished_roots` at
every signal site. The host half is `RuntimeCollectionGate`; the runner's park-window exemption
remains the narrower, complementary cover for the host-held-outside-roots hazard described above.
