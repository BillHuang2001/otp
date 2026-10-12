# A.9 scheduled (`run`) route: the monitor-DOWN signal growth that relocates a woken watcher's heap

Sibling-owned dossier for the A.9 **scheduled (`run`) route** — the two differential corpus cases
`selfhost/in_process_compile_file_no_spawn_deterministic` (E0:
`compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, Dir}])`) and
`selfhost/scheduled_process_compile_file_deterministic` (the default shape through `compile:file/2`'s
own compiler process). Both **agree** with the oracle on both execution tiers; the corpus is
`total=1868 agree=1866 mismatch=0 unsupported=2`. Present state, not a history. The current picture:
one `beam_process` collection site relocates a woken monitor watcher's heap against an incomplete root
set, `beam_rt` declines that collection for the parked/woken window, and the root fix is owed to
`beam_process`.

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

The collection relocates the **monitor watcher's** heap. The watcher was just woken
`Waiting` → `Runnable` by a preceding delivery, so
`Process::has_unpublished_roots()` (= `relocation_exempt || state == ProcessState::Running`,
`beam_process/src/process.rs:1098`) is `false` and the site's gate passes — even though a
woken-but-not-yet-resumed process's `regs` snapshot is **not** its whole live root set (the parked/
woken machine's live terms are not all published). Measured on the traced run: mature base
`0x70876d95dc10` → `0x70875d099090`, nursery used 232 → 20 words.

## 2. Ruled out by measurement
* The valved **delivery receiver-collect**: forcing `RuntimeContext::delivery_collection_is_unsound`
  (`src/context/mod.rs`) to `true` for **every** receiver changed nothing.
* The `beam_rt` admission doors and the park arm: `Runtime::serve_gc_request` (`src/runtime/gc.rs`)
  and `context/gc.rs::collect_or_grow`/`alloc_words_rooted` never fired.
* Any `beam_rt`-level delivery: `deliver_into_mailbox` PRE/POST the receiver heap was unchanged —
  every `beam_rt` pass-boundary delivery already exempts the receiver, because
  `RuntimeContext::for_process` sets `mailbox_pid = pid`.

## 3. `beam_rt`'s mitigation (in place)
The runner's **Wait park arm** (`src/runner/run.rs`) marks the process **relocation-exempt** for the
whole parked window, and the **resume** clears it. A park-window (and woken-but-not-yet-resumed)
collection of that process is therefore declined by `Process::has_unpublished_roots()`, the delivery
takes the ordinary heap-fragment fallback, and the process's own admission point — after the resume,
where the exemption is cleared — collects as usual. The mitigation is **decline-only and bounded**: it
never *allows* a collection, so an affected process's heap grows on that path only until it resumes.
The regression test is `tests/compile_file.rs::a_scheduled_process_parses_a_source_file_like_the_oracle`
(`epp:parse_file/3` driven through the scheduled `run` route over the committed `epp_forms.erl` /
`epp_input.erl` fixtures).

## 4. The patch owed to the sibling owner (root fix)
`crates/beam_process/src/ptab.rs` — give the signal sites (`grow_receiver_for_signal`,
`build_signal_message`, `copy_reason_onto`, `deliver_head_signal`, `enqueue_exit_signal` /
`enqueue_down_signal`) the host's **decline predicate**, or the same **unscanned-root valve** the
`beam_rt` admission doors use, so a signal-driven `ensure_capacity` of a watcher whose root set is
incomplete is **declined** rather than run — the heap-fragment route the cascade already takes for a
`Running` or relocation-exempt receiver. That site is where the refusal belongs; the runner's
park-window exemption then no longer carries this case.
