# `beam_engine` — Machine state core (`src/machine/`)

## Intent
The process-state core of the Stage-1 engine: the register files, the Erlang stack, the call-frame and
exception catch-frame stacks, the module-context registry, the run entries, and `Machine::drive` — the
module-context-aware **dispatch loop that IS the interpreter's hot loop** (the `run_loop*` entries in
`../interpreter/dispatch.rs` are thin wrappers over it).

## API Surface (crate-internal; methods on `impl Machine`)
- `mod.rs` — `Machine` (x-register file, float-register (FR) file of `Eterm` term slots, Erlang stack,
  call frames, exception catch frames, literal area, module-context registry, the **raise-time snapshot**
  `raise_snapshot: Vec<(u32, u32, u32)>` — the pre-unwind `(context, func, pc)` chain, the engine's
  replacement for C's saved raw `StackTrace` (`c_p->ftrace`), left **untouched** by a raise that supplied its
  own trace (nothing is derived at a re-raise site, so the preserved chain is the re-raised exception's) — latched A.8 trace gate, reduction counter,
  the **in-machine BIF-dispatch gate** `bif_in_machine_on` — the cached
  `ExecutionContext::wants_bif_in_machine` answer, latched once per run by `Machine::drive` **and** by the
  native baseline tier's driver, read by `Machine::dispatch_bif_call` — and the fourth latched gate,
  **`census_on`**, the per-run copy of `exec_census::is_enabled()` that the per-instruction funnel at
  `interpreter/dispatch.rs::step_instruction_inner` tests instead of touching the census's thread-local on
  every instruction (cleared by `reset`, latched by both drivers, so a mid-run `enable()` takes effect at the
  run's next entry)); `MAX_REGISTER`/`MAX_FLOAT_REGISTER`; `reset`; the **root-set hand-off**
  (`stack_mut`, `with_roots` handing over the live `RootSet`, and `alloc_rooted` — the convenience
  wrapper that calls `ExecutionContext::alloc_words_rooted`; plus the two **root-enumeration doors**:
  **`for_each_root(&self, impl FnMut(Eterm))`**, the read-only census, which enumerates exactly the
  **pointer-bearing** root words the hand-off covers — `x[..used_x_extent()]`, the whole stack,
  `fr[..used_fr_extent()]` and the five loose heap terms, with every `IMMED1` word skipped by
  `beam_term::primary_tag` — and **`for_each_root_mut(&mut self, &mut impl FnMut(&mut [Eterm]) -> Result<()>)`**,
  its **writable/relocating** twin: the same root words handed over as three `&mut [Eterm]` slices, one at a
  time (`x` prefix, whole stack with the parked loose terms inside it, `fr` prefix, immediates included
  because a contiguous prefix cannot be filtered). Neither covers `args_scratch`, which is `mem::take`n out
  of the machine for the whole wide dispatch; that window is sound only because no rooted door can be
  crossed while it is open (pinned by `tests/bif_args_window.rs` — see `## The loose-heap-term census`)); and
  the **off-heap refc-binary handle view**
  `refc_binaries()` — the read-only slice of the `RefcBinary` payload handles `alloc_refc_binary` built,
  which a host clones into a copy's **source** off-heap table so the copy engine can resolve a refc
  binary's payload by data pointer (a cloned handle's refcount keeps the payload alive past the machine);
  with it the **retention seam** `off_heap_overhead_bytes()` (the reservoir's backing-store bytes) and
  `retain_refc_binaries(keep)` (host-supplied reachability pruning, returning the number of handles
  released) — see `## The off-heap refc-binary retention seam` and the `offheap.rs` bullet.
  Also on `mod.rs`: **`Machine::register_count()`** — *this* machine's X-register file size
  (`x[0..register_count())`, i.e. the bound `Machine::x` indexes into), which is what a caller that walks
  the argument registers must stop at when the file was narrowed with `Machine::with_max_register`;
  reporting the machine's own size rather than the `MAX_REGISTER` constant is what keeps such a walk inside
  the slots that exist. And the class-atom state: `class_atoms: [Eterm; 3]` (indexed by
  `exception_class_index`, `THE_NON_VALUE` = unresolved) plus the **per-run `class_atoms_live: bool` latch**
  described under `catch.rs`.
- `registers.rs` — x/FR access (`get_fr`/`set_fr`, bounds-checked → `bad_register`; `x_mut`/`fr_mut` as
  the single-view root-set accessors); `fr_mark()` — the **live leading FR-slot count** (the FR high-water
  mark, `0` ⇒ no live FR slot), the FR twin of the `x[0..live)` root-set bound, so a host can publish/scan
  only the slots an FR write actually touched. The refill bound is the **used extent** of each file:
  `used_extent(len, mark, dirty)` = `max(mark, dirty)` clamped to the file length, where the mark is raised
  by `set_x`/`set_fr` and the *dirty* extent by every writer that cannot report slots — `x_mut`/`fr_mut`
  (the whole file), `with_roots` (the prefix it hands over) and the native driver (the artifact's own
  `CompiledModule::native_write_extent`, noted **before** the block runs). Every slot at or beyond the extent
  is `NIL` by the refill invariant, which is what makes the narrow root hand-off sound by construction
  rather than by host promise. `Machine::reset` clears the raise snapshot alongside the module contexts (both
  belong to the run that is starting) **and every raise latch** — the four one-shot/live pairs
  `pending_callee_head`/`raise_head_item`, `pending_function_clause_args`/`raise_function_clause_args`,
  `pending_supplied_trace`/`raise_supplied_trace` and `pending_supplied_term`/`raise_supplied_term`
  (`registers.rs:407-421`).
- `mod.rs` — the four raise latch pairs. The failed-clause pair:
  `pending_function_clause_args: Option<Eterm>` (one-shot — armed by the raise site, consumed by the
  `throw_with_trace` that lands it, so a propelling `Err` cannot leave a stale list for a later trace) and
  `raise_function_clause_args: Option<Eterm>` (the **live raise** state, set on every `throw_with_trace` and
  consumed exactly once by the two cooked-trace builders). The explicitly-supplied-trace pair:
  `pending_supplied_trace: Option<StackTrace>` (one-shot, armed only when an incoming `BeamError::Raise`
  carries **non-empty** symbolic frames — the `erlang:raise/3` trace-list shape) and
  `raise_supplied_trace: Option<StackTrace>` (the live state for the raise being landed). The
  supplied-**term** pair: `pending_supplied_term: Option<Eterm>` (one-shot, armed by `Machine::raise_supplied`,
  which `Instr::RawRaise` calls — the shape `sys_core_fold` folds `catch C:R:S -> erlang:raise(C,R,S)` into)
  and `raise_supplied_term: Option<Eterm>` (the live state; `throw_with_trace` promotes the one-shot latch
  into it and then **skips** the `raise_snapshot` rebuild, so the chain of the raise being re-raised survives
  — C's `raw_raise` stores the supplied trace and clears `EXF_SAVETRACE`,
  `../../erts/emulator/beam/beam_common.c:1148-1179`). It is consumed exactly once, by whichever destination
  the raise takes: a **landed** raise publishes it as the handler's `x(2)`
  (`Machine::land_exception`'s `supplied_term` argument, C's register convention), an **uncaught** one renders
  the preserved chain in `Machine::with_uncaught_frames`.
  **Rooting: `Machine::with_roots` (the single root hand-off behind `alloc_rooted`/`ensure_heap`, hence behind
  every term-allocation and heap-admission door on every tier) PARKS the machine's loose heap terms**
  (`Machine::loose_heap_terms`, in this order: `raise_function_clause_args`, `pending_function_clause_args`,
  `pending_exception`'s reason, `pending_supplied_term`, `raise_supplied_term`) **onto the tail of
  `self.stack`** for the duration of the hand-off, hands over the `RootSet`, then **re-seats** each field from
  its slot (`self.stack.get(cursor).copied()` — the stack is rooted in full, so a conforming host relocates
  the parked word and the engine reads the *moved* term back), and finally `stack.truncate(base)`.
  With every loose term `None` the `RootSet` is handed over **unmodified** (byte-identical fast path).
  A `None` read-back leaves its field untouched rather than clearing it (a host would have to move a slot out of
  the `&mut [Eterm]` it was handed to produce one).
  The source of truth for the parked set is the pair `Machine::loose_heap_terms`/`Machine::seat_loose_heap_terms`
  plus the parking loop `Machine::with_parked_loose_terms`, so a future loose field is added in exactly one place.
  The `Option<StackTrace>` pairs (`pending_supplied_trace`/`raise_supplied_trace`) are **metadata**, never a
  heap term, so they are not parked; both **term** pairs are — the failed-clause pair and the
  supplied-trace pair — so neither depends on an "allocation-free window" argument any more.
  **`Machine::pending_exception` IS parked**, so its reason is a live root at every door: a word a host reads from
  the field has already been relocated by every collection that preceded the read, which is what makes `beam_rt`'s
  park publication (`Registers::extra_roots`, read back through `Machine::pending_exception_mut`) and the in-crate
  `families::bitstrings::raw::payload_aliased_elsewhere` read safe by construction.
  The pair is **retained past landing** (C clears `c_p->fvalue` in `handle_error`,
  `../../erts/emulator/beam/beam_common.c:527`), which is sound precisely because the reason stays rooted here —
  C roots `fvalue`/`ftrace` for the window they exist (`setup_rootset`/`offset_rootset`,
  `../../erts/emulator/beam/erl_gc.c:2676-2693`, `:3513-3514`).
  **Parking is unconditional** — an *immediate* reason (the common `function_clause`/`badarith` atoms, which no
  collector can move) is parked too, keeping one uniform rule instead of a tag test at the door. Consequence: the
  byte-identical no-loose-term fast path is unavailable for the **rest of a run that has caught an exception**
  (`pending_exception` is cleared only by `Machine::reset`/the next raise), so every later door pays at most five
  extra option reads plus the push/get/truncate of the armed words. Gate the reason slot on
  `beam_term::is_immediate` in `Machine::loose_heap_terms` if a measurement ever shows that cost.
  **Host hand-off invariant (publish at a park → scatter at the resume).** The slice a host receives is
  `roots.stack.len() == machine.stack.len() + armed_loose_terms`, and `armed_loose_terms` is **0..=5 and
  state-dependent** (`loose_heap_terms`: `raise_function_clause_args`, `pending_function_clause_args`,
  `pending_exception`'s reason, `pending_supplied_term`, `raise_supplied_term`), so a publish/scatter pair is
  length-correct only while **nothing touched the machine in between** — no instruction ran, no raise landed
  (`throw_with_trace` sets `pending_exception`/`raise_function_clause_args`/`raise_supplied_term`, `catch.rs`),
  no `Machine::reset` (which clears all five *and* `stack`).
  A host **cannot** compute or check that count: `loose_heap_terms`/`LOOSE_HEAP_TERMS`/the two clause latches are
  private (`pending_exception`'s presence is the only observable part) and `Machine::stack_depth()` is
  unreachable from inside the closure (the machine is mutably borrowed).
  So a host must round-trip `roots.stack` verbatim and treat a length mismatch as a contract violation — never as
  a licence to keep words the collector may have left stale.
  `roots.x.len()`/`roots.fr.len()` (`used_extent`) can only **grow** between the two calls (any `set_x` on a
  higher register raises `x_mark`), so a host must record the published lengths rather than recompute them.
  The variance is pinned by `tests/raise_rooting.rs::the_latch_free_run_hands_over_an_unmodified_root_set`
  (one identical frame shape: 2 stack words unarmed, 4 with a raise armed).
- `frames.rs` — the frame push/pop pair (`Machine::push_frame`, and `Machine::push_frame_window`,
  the same push that also hands back the pushed frame's `(y_base, y_count)` — the push has just
  materialised both figures, so a caller that must publish the Y window need not re-derive it from
  `frames.last()`; `pop_frame`, and the size-checked `pop_frame_checked`, whose single `Vec::pop`
  **is** the only read of the top frame — a mismatched `y_count` is the cold arm and restores the
  identical frame, since `Frame` is `Copy`), `allocate*`/`deallocate*`/`trim`, the typed
  `Continuation` CP slot (`Machine::continuation` read-only, `Machine::take_continuation`
  read-and-clear, `Machine::set_continuation` write), `Machine::ensure_heap`
  (the `ensure_heap_full` root-set admission point that hands over the whole `RootSet`), and the
  caller-chain introspection (`resumption_contexts`/`resumption_addresses`/`raise_snapshot`).
- `catch.rs` — the `CatchFrame` stack + the raise/unwind entry (`throw`/`throw_error`/`throw_with_trace`)
  and the **uniform raise funnel**: `throw_with_trace` first REBUILDS `raise_snapshot` (the raising frame
  from `current_code`/`current_context`, then every live caller from `resumption_contexts`) — **before** any
  unwind and before the early no-live-frame `Err(BeamError::Raise)` return, so the pre-unwind chain
  survives for the handler. It also promotes the failed-clause argument list: it takes the one-shot
  `Machine::pending_function_clause_args` and stores it as the live `Machine::raise_function_clause_args`
  (a raise that carries no such list clears it, so no later trace can inherit a stale one); it does the same
  for `Machine::pending_supplied_trace` -> `Machine::raise_supplied_trace` and for
  `Machine::pending_supplied_term` -> `Machine::raise_supplied_term`. It also clears the landing frame's
  continuation slot (`set_continuation(Continuation::Halt)`, the handler runs in that frame) and then hands
  `land_exception` the trace the handler must see in `x(2)`: the supplied **term** when this raise carried
  one, else the raise's own raw-trace word.
  **A supplied trace is preserved, a derived one is built here.** The `raise_snapshot` rebuild runs only
  when the raise supplied **neither** flavour (a supplied symbolic trace renders its own frames; a supplied
  trace term renders the preserved snapshot of the raise being re-raised, so rebuilding would replace it with
  a re-raise-site chain) — see `## A frame's continuation slot is consumed by whoever follows it` and
  `../families/exceptions/CONTEXT.md`. An **uncaught** raise still leaves through the frameless-`Raise` arm
  (either the snapshot rebuild above has run, or the supplied trace is what the run renders), and
  `Machine::with_uncaught_frames` attaches the frames at the run boundary: the live supplied-symbolic latch
  when one is armed, else `Machine::snapshot_stacktrace` for the supplied-term case, else
  `Machine::raise_stacktrace`.
  **The exception-class atoms and `namespace_atom`.** `Machine::ensure_class_atoms(ctx, module)` resolves every
  still-unresolved `error`/`exit`/`throw` atom from the engine's live atom namespace and installs it with
  `Machine::set_class_atom`; an atom already installed is never overwritten, and the per-run **`class_atoms_live`
  latch** makes the whole body one `bool` test in steady state (it is cleared by `Machine::reset`, because the class
  atoms belong to the run that is starting). `Machine::class_atom` then reports the typed
  `unsupported("exceptions: no atom for the '<class>' exception class …")` for a class neither source can name —
  never a substituted atom. The name resolution itself is the free function `namespace_atom(ctx, module, name)`:
  `ctx.atoms(module)` (the host's live namespace seam) first, the running module's load-time snapshot as the
  fallback, `None` when neither knows the name. C reads its `am_error`/`am_exit`/`am_throw` **globals**, and the
  seam is the engine's equivalent: `beam_rt` answers with its ONE live shared table, while a host that does not
  override `atoms` keeps the module snapshot (byte-identical to the pre-seam behaviour). The `atoms` borrow is
  immutable and ends at the `Copy` `Atom` it yields, so a caller may make its next `&mut ctx` call right after.
  `raise_stacktrace(entry_module: Option<&Module>, func, pc, first_frame_args: Option<Eterm>) -> StackTrace`
  derives the symbolic `StackTrace` (raising function, then every live caller innermost first), giving
  `first_frame_args` to the **raising** frame only; `Machine::with_uncaught_frames(&mut self, err, entry_module, func, pc)`
  attaches it to an uncaught `BeamError::Raise` (leaving a non-raise error, an already-framed raise and an
  **engine diagnostic** — a reason atom prefixed `unsupported_opcode: `, the `error::unsupported` sentinel —
  untouched). The funnel's first half runs at the `step_instruction` boundary (`../interpreter/dispatch.rs`),
  its **second half at the `drive` boundary** for a raise that surfaced inside a cross-module callee (`drive.rs`).
  The live latch splits around `symbolic_stacktrace`'s one rooted reservation: the **read** half
  `Machine::live_raise_function_clause_args()` yields the list *before* it (the sizing needs only the
  list's move-invariant **length**) and the **consuming** half `Machine::take_raise_function_clause_args()`
  (called **exactly once** by each of the two cooked-trace builders) consumes it *after* it — the
  reservation may have moved the list, and `with_roots` re-seats the field
  from the slot the host wrote the moved word back through — handing the re-seated list to the first
  frame's `A` slot (every outer frame keeps its declared arity); the list's elements are carved **inside**
  the reservation, so no element word is held across a collection. `Machine::take_raise_supplied_trace()` is the
  consuming half of the symbolic supplied-trace latch (`symbolic_stacktrace` renders those frames **instead
  of** the snapshot heuristic when it is armed, and an absent/empty supplied trace leaves every path
  byte-identical) and `Machine::take_raise_supplied_term()` the consuming half of the **term** latch — whose
  consumer is not `symbolic_stacktrace` but the landed `x(2)` publication or the uncaught funnel, since a
  supplied term is already a cooked `[{M,F,A,Location}]` list and `symbolic_stacktrace` returns such a list
  from the trace register verbatim (`is_list`, C's `is_raised_exc` arm,
  `../../erts/emulator/beam/beam_common.c:1219-1223`).
- `context.rs` — the `ModuleContext`/`Contexts` registry; `current_context`/`register_context`/`switch_context`/
  `module_context`.
- `offheap.rs` — the off-heap refc-binary **retention seam**: `Machine::off_heap_overhead_bytes()` (the sum of
  `RefcBinary::reserved_bytes()` over `refc_binaries`) and
  `Machine::retain_refc_binaries(keep: FnMut(*const u8) -> bool) -> usize` (drops the handles the predicate
  rejects, returning how many were released, and prunes `writable_payloads` in lockstep). It is a separate
  file to keep both modules under the line budget; a child module of `machine` sees the parent's fields.
  Detail: `## The off-heap refc-binary retention seam`.
- `local_callee.rs` — the **local-call memo** `Machine::local_callees` (`LocalCallees`, `CalleeKey`,
  `CalleeTarget`, `Verdict`): **both** halves of what the local-call seam
  (`../interpreter/dispatch.rs::local_call_seam`) needs — the `(module name, function name, arity)`
  derivation *and* the host's `resolve_local_call` verdict — memoized per `(module address, function
  index)` in a fixed `CAP = 8` cache held as three parallel slices: a compact `keys: [CalleeKey; 8]`
  (16 bytes per slot), a `verdicts: [Verdict; 8]` byte array (the only other thing the hot path reads)
  and the `targets` payloads that own their name text (cold paths only).
  `Verdict` is `Unnameable` (no function entry / unresolvable name atom — the host is asked **zero**
  times; it doubles as the vacant-slot value) / `Ask` (fill or invalidated) / `Served` / `Declined`, so
  the seam's hot path is `lookup`'s single scan over the **occupied prefix** `keys[..len]` plus one
  bool: a repeat local call makes **no** host call at all.
  **Policy: insert-at-front on a miss; a hit never reorders** (the entry keeps its slot), and the
  oldest *insertion* falls off the end. That is deliberate: `list_walk`'s inner loop alternates two
  callees (`sum/1`, `walk/3`), so an MRU move-to-front fired on every call and memmoved up to eight
  owned entries in the innermost loop.
  **Invalidation** (the host's answer set is a pure function of the triple for `beam_rt` — `prim_tty`'s
  stubs plus `beam_bifs`' `zlib` table — so only an *installation* can change it):
  `erlang:load_nif/2` (the BIF-dispatch funnel `Machine::dispatch_bif_call`, `../interpreter/bif.rs`, **arms**
  `Machine::local_call_invalidation_pending` — a `Cell<bool>` written through the funnel's `&self` receiver —
  when the dispatched triple is `("erlang","load_nif",2)`, one arity compare on the cold path; the seam clears
  the verdicts at its next local call, which is where `&mut self` is available), [Machine::reset] (which clears
  the whole memo, keeping the address key ABA-free; a resume does not reset) and the **public**
  `Machine::invalidate_local_call_verdicts` door, which a host whose answer set changes by any other means (its
  own plugins, a reload, a config toggle) calls itself.
  Invalidation re-asks the host but **re-derives nothing** — identities are pure functions of an
  immutable module image and cannot go stale.
- `location.rs` — execution-position introspection (`current_code`, `current_function`, `current_location`,
  `initial_call`, `current_stacktrace`, `current_module`/`active_context`) **and** the parked-`receive`
  deadline seam `Machine::wait_deadline`/`timeout_delay` + `WaitDeadline { delay_ms, resume }`: it reads the
  `wait_timeout` at `current_code` (the blocked instruction, exact because a blocking op is never JIT-inlined)
  in the module `active_context()` names — the callee's own module when the park is inside a cross-module
  callee (context != 0), else the passed entry module — and reports the `timeout` continuation (`pc + 1`) a
  host re-enters when its own timer fires. The position itself is **one packed `u64`**
  (`(func as u64) << 32 | pc`, `pack_current_code`/`unpack_current_code`, `CURRENT_CODE_NONE = u64::MAX`
  with the `func == u32::MAX` sentinel), so `Machine::set_current_code` is a **single store** and every
  reader — `current_code`, `current_function`, `current_location`, `current_stacktrace`, `wait_deadline` —
  goes through the unpacker. `Machine::timeout_delay` accepts every operand shape the compiled
  `wait_timeout` can carry: an `Integer`/`Literal` delay, an `X`/`Y` register, and an `Operand::Typed { reg, .. }`
  (the `{tr,..}` typed-register operand, unwrapped to its `X`/`Y` register — the type annotation is a
  compiler hint, not a heap type). The engine arms no timer: a non-zero `wait_timeout` just suspends.
- `run.rs` — `run_export`/`run_label` (non-blocking) and `run_export_outcome`/`run_label_outcome`/`resume_outcome`
  (blocking-aware); the **tier-selecting** entries `run_with_tier`/`run_export_outcome_with_tier`/
  `run_label_outcome_with_tier` (first entry) and `resume_outcome_with_tier` (**resume** — a slice that suspended on a
  native tier resumes on it: `Interpreter` → `resume_outcome`, `BaselineJit` → the cached no-reset baseline resume,
  `Cranelift` → a typed error); `RunOutcome` (`Returned` / `Wait` / `Suspend`), re-exported at the crate root;
  `fold_outcome` is the non-blocking entries' single fold — `Wait` → `error::blocked()`, `Suspend` →
  `error::suspended()`.
- `drive.rs` — **`Machine::drive`**: owns the `(func, pc)` position *and* the module context those indices belong
  to, folds every `Step` back into that position, and charges **exactly one reduction per interpreted instruction
  before the instruction's effect** through the defaulted `ExecutionContext::reduce` (the pre-emption point a
  non-allocating loop reaches on this tier — the native baseline tier's driver loop charges the same hook once per
  block entry instead, see `../jit/baseline/CONTEXT.md`; read back through `Machine::reductions()`/`reset_reductions()`). The charge precedes
  the body (C's `DISPATCH` order), so an `Err` from the hook pre-empts the instruction **without applying it**: the
  machine is left parked at that `(func, pc)` — recorded by `Machine::set_current_code` before the charge — and a
  `Machine::resume_outcome` there applies it exactly once. It also latches the **in-machine BIF-dispatch gate**
  (`bif_in_machine_on`, the cached `ExecutionContext::wants_bif_in_machine` answer) so every BIF dispatch in the run
  goes through the single `Machine::dispatch_bif_call` funnel, and threads the run's **entry module** (context `0`)
  down to it via `step_instruction_with_entry` — the same latch the native baseline tier's driver performs. It then re-binds the
  literal area exactly on a `Step::Enter` into a different context. It is also where the two parks are folded:
  `Step::Wait` → `RunOutcome::Wait` (the `receive` resume label) and `Step::Suspend` → `RunOutcome::Suspend`
  (the **un-applied** BIF instruction itself, re-executed on resume, the machine left as the pre-execution
  state). It also runs the raise funnel's second half
  (`Machine::with_uncaught_frames`) over a raise that surfaced inside a cross-module callee — the driver is where
  the run's true entry module is known, so those caller frames are named against the right module.
- `tests.rs` — the cross-cutting end-to-end tests of the raise funnel: one synthetic module per raise kind
  (`throw`/`error`/`exit` `raw_raise`, a BIF error, `func_info`, a nested call, a cross-module callee), each run
  through a real run entry and asserting the propagated `Err(BeamError::Raise)` frames name the raising function and
  every live caller innermost first, plus the guard that a caught raise lands unchanged. It also pins the
  local-call memo against a counting host: two consecutive local calls to one callee consult
  `resolve_local_call` **once** (a different callee consults again), `invalidate_local_call_verdicts`
  and `reset` each force a re-consultation, dispatching `erlang:load_nif/2` invalidates while an
  ordinary BIF dispatch does not, the identity is derived **once**, the memo holds the callee's own
  names, and a served `call_last` pops its frame like the in-machine route. `test_support.rs`
  (`cfg(test)` only) is the synthetic-module builder (`Fixture`) and cross-module seam context (`seam`) these share
  with `drive.rs`'s tests.
- `root_tests.rs` — the pins for both root-enumeration doors. For `Machine::for_each_root` (the read-only
  pointer-bearing census): coverage of every rooted file plus the five loose terms in the documented order,
  the immediate filter (a machine whose slots hold only small ints/atoms/`NIL` reports nothing, and one
  boxed word in an otherwise immediate-only machine is the only thing reported), and **agreement with
  `with_roots`** — the pointer-bearing word set the walk reports equals the one collected from the
  `RootSet` slices for the same machine state, with the filter written out independently on both sides.
  Plus `with_roots_re_seats_the_two_supplied_trace_term_latches_from_their_parked_slots`: both
  supplied-trace **term** latches armed with cons cells, a hand-off whose visitor *relocates* every parked
  word, and the fields asserted to hold the **moved** terms afterwards (the stack restored, the walk
  reporting the moved words, and `with_roots`/`for_each_root` still agreeing).
  **The parity gate between the doors** (a change to one that is not mirrored in the other fails here):
  `for_each_root_mut_enumerates_the_same_roots_as_for_each_root` (three slices, one visit each; the same
  sorted multiset of pointer-bearing words as the census — order differs because the loose terms ride the
  stack slice — with the fixture's immediates and the parked loose terms asserted to sit inside the right
  slice); `for_each_root_mut_relocates_the_very_slots_for_each_root_reports` (a moving visitor through the
  writable door, after which the census reports exactly the relocated words and no abandoned one);
  `for_each_root_mut_writes_are_observed_by_for_each_root` (every pointer word replaced by a distinguishable
  **immediate** marker ⇒ the census reports nothing, a second visit checks slot-for-slot that the marker sits
  where the census's word was and every immediate is byte-identical, then the originals are restored and the
  census's multiset returns); and
  `for_each_root_mut_excludes_args_scratch_and_restores_the_parking_on_error` (the handed slices cover
  exactly the `x` prefix + the stack with the armed loose words parked in it, none of their byte ranges
  overlaps the `args_scratch` buffer, and an `Err` from the stack slice stops the walk yet still truncates
  the stack and re-seats all five loose fields).
- `deadline_tests.rs` — the parked-`receive` deadline driven to completion: a **nested** run through the
  non-blocking `run_export` (whose fold to `blocked()` must leave the machine parked, so `wait_deadline` still
  reports the deadline) and a blocking `run_export_outcome` park, each resumed at the deadline's `timeout`
  continuation to run the `after` clause — on a real module compiled by the system `erlc` and cross-checked
  against `erl` (loud skip when `erlc`/`erl` are absent); plus a cross-module-callee park proving the deadline
  is read from the callee context, never the entry module.

## The loose-heap-term census, the hand-off width rule and the parked-park seams
What the root set is **made of** and what it provably leaves out — the invariants a change to any
`Eterm`-typed `Machine` field must not silently break.
- **The term-bearing fields.** The frozen `RootSet` (`x[..used_x_extent]`, the whole `stack`, the whole `fr`),
  the five **loose** fields the root set cannot carry (`loose_heap_terms`, `mod.rs:937-945`, in the fixed order
  `raise_function_clause_args`, `pending_function_clause_args`, `pending_exception.reason`,
  `pending_supplied_term`, `raise_supplied_term`), and `args_scratch`.
  Everything else that is `Eterm`-typed is an **immediate-only** file (`class_atoms: [Eterm; 3]` — atoms; the
  two `Option<StackTrace>` latches — metadata) or off-heap (`refc_binaries`, `writable_payloads`), which no
  collection relocates.
- **`args_scratch` (`mod.rs:574`) is `mem::take`n out of the machine for the whole wide BIF dispatch**
  (`interpreter/bif.rs`, restored there; the same shape in `interpreter/trace.rs`), so no `with_roots` hand-off
  can see it, and **both** root-enumeration doors (`Machine::for_each_root`, the read-only census, and
  `Machine::for_each_root_mut`) deliberately leave it out. That
  is sound only because the dispatch window **cannot collect**, and the reason is structural rather than a host
  promise: `dispatch_bif_call` (`interpreter/bif.rs`) takes **`&self`**, while both rooted doors
  (`alloc_words_rooted`, `ensure_heap_full`) take `&mut self` (the *context*) plus a root set whose slices come
  from *this* machine — so the borrow checker forbids crossing one while the machine is immutably borrowed, and
  the only heap door the marshalling itself uses is the non-collecting `alloc_words` (a non-small integer
  operand). `tests/bif_args_window.rs` pins it: a recording context counts every
  `alloc_words_rooted`/`ensure_heap_full`/`ensure_heap` crossing over a narrow (`arity 2`) and a wide
  (`arity 5`) `call_ext` run and requires **zero** (with a `test_heap` fixture as the live-probe control), and
  the same suite asserts the host is handed exactly `x(0..arity)` on both doors. **Break any of the three legs
  — `&self` dispatch, no rooted door in the marshalling, a non-collecting `alloc_words` — and the argument
  words must be rooted** (kept in the machine and enumerated, or parked at the stack tail), never left to the
  comment. The buffer is reinstated **empty** on every exit path (no `?` between the `take` and the reinstall),
  pinned by the `bif` unit test beside the wide-marshalling proof.
- **Two root-enumeration doors, one parking implementation.** `Machine::for_each_root_mut` — the writable
  (relocating) door — IS one `with_roots` hand-off: same `used_x_extent`/`used_fr_extent` widths, same
  `note_x_dirty`/`note_fr_dirty` notes, same `with_parked_loose_terms`/`seat_loose_heap_terms` discipline, and it
  visits the `RootSet`'s three **disjoint** slices one at a time (so the visitor cannot alias one root file while
  it is handed another). Its error path is the parking's too: the walk stops at the failing slice (the later ones
  are never handed over), the stack is truncated and all five loose fields re-seated, and only then does the
  visitor's `Err` travel on. A visitor may rewrite every word, so it must leave a static-literal
  (`TAG_LITERAL_PTR`) word alone and must treat the machine's terms as re-read after the visit (a raw
  `*mut Eterm` from before it is dead). `Machine::for_each_root` stays `&self` — it copies, so it can only
  census — and the two doors' **coverage** is pinned in parity by `root_tests` (see that bullet), both keeping the
  `args_scratch` exclusion above.
- **The hand-off width is `used_x_extent()`/`used_fr_extent()` = `max(mark, dirty)`** (`registers.rs:108-110`),
  never `RootSet::live` (a hint only) — so `live` may be larger than the handed prefix and the slot set is still
  exact. The width is sound only while **every** writer reports: `set_x`/`set_fr` raise the mark
  (`registers.rs:251-288`), `x_mut`/`fr_mut`/`with_roots` raise `dirty` (`:139-142`), and a native block reports
  through the artifact's `native_write_extent` (`jit/baseline/mod.rs:696-702`, `code.rs:615-620`) — the one claim
  that is a **static property of the emitter** (`highest_native_register` must bound every generated store) and so
  cannot be verified by reading the runtime alone. An under-reported extent shortens the hand-off and leaves a
  live register unrooted, which surfaces as a stale word dereferenced later, not as a collector error.
- **The parked hand-off re-derives its two widths at both ends.** `publish_parked_roots` publishes
  `roots.x.len()` + `roots.stack.len()` (machine stack **plus the armed loose words `with_roots` just parked**) +
  `fr_count`, and the resume's `scatter_roots_full` compares against the *then-current* `roots.x.len()` and
  `roots.stack.len() + fr_count`. Nothing inside the engine mutates `x`/`stack`/the loose-arm set while the machine
  is parked (no engine code runs), so the pair agrees; a host that writes `pending_exception_mut().1` before the
  resume, or that resumes a machine it reset, changes one side only.
- **A failed scatter is not a refusal — it is a dangling machine.** A `scatter_roots_full` width mismatch makes the
  host `halt_process_gc` and *resume anyway* (`beam_rt`'s `resync_parked_roots`), while the from-space has already
  been evacuated: every `x`/`stack` word then names freed memory and the first `terms::cons_head`/`cons_tail`/
  `get_list` on it faults inside `beam_term::list::car`. The engine contributes the width rule above; the
  turn-it-into-an-error decision lives in the host node.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no panic, no `unwrap()` on
  runtime data; an unsupported opcode/operand is the documented `error::unsupported`.
- `unsafe` only where the crate `CONTEXT.md` marks it, each block behind a `// SAFETY:` rationale.

## Notes for Agents
- **`Step::Enter` is the only way a run changes module context.** All three producers switch the context at the
  driver: a cross-module `call_ext*`, a cross-module `return` onto a `Continuation::Address` in another context, and
  a raise landing in another context's live catch frame (pinned by `drive.rs`'s tests
  `a_cross_module_return_switches_the_machine_back_into_the_caller_context` and
  `a_raise_landing_in_a_callers_catch_frame_switches_into_the_callers_context`).
- `reset` (`registers.rs`) clears the register files, stacks, catch frames, contexts, the local-call
  memo (`local_callees` — the callee identity **and** the host's verdict; its entries are address-keyed,
  so they belong to the run that filled them, and that is also what makes the key ABA-free), the raise
  snapshot, the pending
  exception, the exception-class atoms + their `class_atoms_live` latch, the execution-position fields and the
  trace/reduce gates, but **not** the `refc_binaries` arena, the `writable_payloads` registry, or
  `bif_in_machine_on` (latched per run into the machine by both drivers; only `Machine::new`,
  `machine/mod.rs`, ever sets it); a resume (`resume_outcome`) resets nothing, so the contexts and the raise snapshot
  both survive it. The two off-heap rosters are released only by a host call to
  `Machine::retain_refc_binaries`.
  **Nothing is ever released or shrunk**: the register/FR files are `fill_nil`-refilled only over their *used
  extent* (never `truncate`d, never reallocated once the length matches), `stack`/`frames`/`catch_frames`/
  `raise_snapshot`/`args_scratch`/`refc_binaries`/`writable_payloads`/`label_maps.entries` all use
  `clear`/`truncate`/`pop`, which **keep capacity**, and no `shrink_to_fit`/`shrink_to`/`reserve_exact` exists
  anywhere in the crate — so a machine's resident footprint is the high-water mark of the heaviest run it has
  ever executed and only dropping the machine frees it. `Machine::new` itself (`mod.rs`) allocates both
  1024-word register files (8 KiB each) before the first run.
  **The three frame vectors are reserved up front** (`STACK_RESERVE_WORDS = 256` words,
  `FRAMES_RESERVE = 256` frames, `CATCH_FRAMES_RESERVE = 8`, via `Vec::with_capacity` in
  `Machine::with_max_register`), so the frame push/pop pair — every non-tail call on every tier — stays off
  `RawVec`'s growth arm on a machine's first run too; because `reset` only `clear`s them, the capacity a
  deeper run grew to survives into the next one. The floors are deliberately modest (2 KiB + 8 KiB + a few
  hundred bytes), so a deep recursion or a nested-`try` storm still grows by the usual doubling.

## What the machine owns (and what it does not)
`Machine` owns **no process heap**: its only non-term field storage is `Arc<Module>`/`Arc<LiteralArea>` handles plus
`Vec`s of words, and every `alloc_words`/`alloc_words_rooted`/`ensure_heap*` call allocates into the caller's
`ExecutionContext` (`MinimalContext.heap: ContextHeap` here; `beam_rt`'s collector in production).
There is **no `impl Drop for Machine`** (the crate's only `Drop` is `jit::emitter::CodeBuffer`): dropping a machine
drops `refc_binaries`' `RefcBinary` handles, each of which decrements and frees its payload only when it was the last
handle (`beam_alloc/src/binary.rs:511`); `writable_payloads` is `(usize, usize)` metadata that frees nothing.
`Machine::reset` frees **no heap word and no payload**, so a returned or host-held `Eterm` outlives a run but never
outlives the `ExecutionContext` whose heap it was built in — a `ContextHeap` region is unmapped only on context drop.
A **nested run on the same machine is destructive**: `run_label_outcome` calls `reset()` (`run.rs:388`) and nothing
saves the outer frame; `resume_outcome`/`run_loop` is the only state-preserving entry (no re-entrancy guard exists).

## A frame's continuation slot is consumed by whoever follows it
`Machine::take_continuation` (`frames.rs`) is the **one** read-and-clear: it reads the current frame's
`Continuation` slot and resets it to `Continuation::Halt` in the same step (C's `RETURN()` macro does
`SET_I(cp_val(*E)); *E = NIL`, `../../erts/emulator/beam/emu/macros.tab:241-247`).
A frame's slot means exactly one thing — *this frame is suspended inside a call and resumes at that address
when the call returns* — so whoever follows the address clears it, and every reader may treat a non-`Halt`
slot as a **live** caller.
- **The complete consumer set** (each is a `take_continuation` call site): `return_step`
  (`../interpreter/dispatch.rs`, the interpreter's `return`/`deallocate_return` arm),
  `Machine::continuation_step` (`drive.rs`, the `Step` protocol's follow), and `jit_return` /
  `jit_deallocate_return` (`../jit/baseline/runtime.rs`, the native baseline tier's twins of those arms).
- **A raise landing in a frame's catch handler clears that frame's slot too** — `Machine::throw_with_trace`
  ends its landing with `self.set_continuation(Continuation::Halt)`, because the handler runs *in* the
  landing frame, which is therefore no longer suspended in the call that raised (C's `next_catch` drops the
  frames above the catch frame for the same reason).
- **The remaining readers are read-only and never reset it**: `continuation()` (the test-only observation
  view), `resumption_contexts`/`resumption_addresses`, `raise_stacktrace`/`snapshot_stacktrace` and
  `current_stacktrace`.
- **The writers are unchanged**: `Instr::Call`, `call_ext*`, the `Apply*`/`call_fun` arms and the baseline
  tier's `store_continuation` (`../jit/baseline/call.rs`) arm the slot through `set_continuation`.
Consequence: a caller that already returned is never named a second time, so a derived stacktrace names the
raising function once and thereafter only its **live** callers.
C's `gather_stacktrace` (`../../erts/emulator/beam/beam_common.c:782-842`) likewise admits only words that
still look like a CP.

## The cross-module call seam (`resolve_external_call`) and `Machine::drive` (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **The cross-module call seam is additive and host-driven.** `resolve_external_call` (default `Ok(None)`) lets a host that owns loaded modules run a `call_ext*` **in the same machine**, so a `receive` inside the callee suspends the driving process (`RunOutcome::Wait`) instead of folding into `Err(error::blocked())`. The mechanism is C's model: a code pointer (the frame `Continuation`) carries its module context, so a `return`/raise can land in another module; the driver (`Machine::drive`) owns the `(func, pc)` position + its context and re-binds the literal area only on a `Step::Enter` to a different context. Modules are held as `Arc<Module>` handles deduped by `Arc::ptr_eq` (bounded by distinct modules, not call count) and cleared only by `reset` -- never by a resume, because a suspended cross-module frame must still name its module. The seam executes on the native baseline tier through a **driver-mediated module-context switch**: a resolved transfer is not in the JIT inline set, so the block deopts one step, that step hands the driver a `Step::Enter` (the `ENTER` block-ABI word), and the driver switches module context and re-enters the callee's compiled blocks (when the tier has them). A seal-time cross-module *link* is unsound (separate executable mappings, buffer-local `rel32` fixups, the entry module baked into `JitRuntime`), so the transfer stays a driver round-trip by design.

## Reductions and pre-emption: the charge site, the parking contract and the native tiers (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **Reductions and pre-emption exist; the scheduler does not.** `Machine::drive` charges one reduction per interpreted instruction through the defaulted `ExecutionContext::reduce(n)` hook (`Ok(())`), **before** the instruction's body runs (`Machine::drive` records `(func, pc)` via `set_current_code` and charges the hook *before* `step_instruction`) and at a single call site **in that driver**, so the interpreter's charge is exactly one instruction (a **non-allocating** loop, which never reaches `ensure_heap`, is still pre-emptible). A host with a scheduler pre-empts by returning a **typed error** from `reduce`, which the run reports exactly like any other step failure; the pre-empted instruction is left **un-applied** and the machine parked on it (`Machine::current_code`), so a `Machine::resume_outcome` at that address applies it **exactly once**. `Machine::reductions()` reads the current slice's count (zeroed at slice entry; one `drive` call = one slice, so `resume_outcome` starts a fresh slice) and `Machine::reset_reductions()` returns it while zeroing it. There is **no `RunOutcome::Yield` and no `ReduceOutcome`**: pre-emption is an `Err` out of `reduce`, resumption is a fresh `drive` (`tests/reductions.rs`). The scheduler, run queues, trap handling and `ensure_heap` limits are Stage 5 (`beam_process`); stack growth is an unbounded `Vec`.
- **The native tiers charge the same hook once per driver-loop iteration.** The JIT driver (`src/jit/baseline/mod.rs::drive`, shared by `Tier::BaselineJit` and `Tier::Cranelift`) latches `Machine::reduce_on = ctx.wants_reduce()` and zeroes `Machine::reductions` at its entry (one `drive` call = one slice, as above), then at the top of every iteration publishes the position with `Machine::set_current_code(func, pc)`, bumps the slice counter and -- gate on -- calls `reduce(1)?`, all **before** `code.enter(...)`. An iteration is one **block entry**: a natively executed block, or the per-instruction interpreter step the driver takes for an address no block covers. That granularity is what makes the refusal a pre-emption -- the runner parks an exhausted machine at `Machine::current_code` and re-enters there, and at a block entry the published address *is* the one about to run with the block unexecuted, so the resume executes it exactly once. A refusing charge from the call-resolution/BIF seams would refuse with the block's entry position current only after part of the block had run and the resume would replay it (measured: `beam_rt -eval 'halt(0).'` exits 3 with `unsupported: opcode: run: process blocked`), so those seams stay spend-only. A host whose `reduce` is the default no-op pays one `bool` load per iteration; the counter is bumped either way. Residual: a **natively linked local tail-call chain** (`call_last`/`call_only` linked between compiled blocks) stays inside one driver entry, so it is priced once and, having no refusing seam inside it, cannot end its slice from inside -- `slice.beam`'s `fib/1` at n=18 costs 2 charges against the interpreter's 152 (`tests/jit_reductions.rs`).

## The off-heap (refc) binary handle: pricing and pruning (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **An engine-built off-heap (refc) binary reaches the host as a *handle*, never as raw memory.** `families::bitstrings::raw::{make_writable,make_binary}` build the frozen 8-word `[BinRef][ErlSubBits]` pair and push each payload handle onto `Machine::refc_binaries` (never cleared by `reset`); the `send` arm and the `erlang:send/2` special case hand the in-flight slice to the host through `send_message_with_off_heap`, and `beam_rt`'s runner parks the copied payload references in its destination off-heap table, so an engine-built refc binary survives both a **return** and a **send**. The reservoir is priced and pruned by the host through `Machine::off_heap_overhead_bytes()` (backing-store bytes; words = bytes / `size_of::<Eterm>()`) and `Machine::retain_refc_binaries(keep)` (drop every handle a reachability predicate rejects, returning the released count; retained payloads keep their `data_ptr` and the `writable_payloads` registry is pruned in lockstep), so the earlier accounting gap is closed at the seam. Pinned by `tests/stdlib_sweep.rs::sweep_base64_encodes_the_empty_binary`, `src/families/bitstrings/matching/mod.rs::empty_binary_bs_match_commands_and_the_writable_accumulator`, `src/families/receive.rs::send_forwards_the_machines_off_heap_binary_handles`, and `src/machine/offheap.rs`'s unit tests.

## Flat, integer-keyed hot-path state (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **Flat, integer-keyed hot-path state (safe Rust).** Jumps resolve through `LabelMap` -- a dense `Vec<Option<(func,pc)>>` (`Slots::{Dense,Sparse}`) built once per run by `Machine::enter_run` (the loader's label ids are dense: `lists.beam` has 1256 labels, max id 1256) -- so a jump is one array index. Each import's `call_bif` target is resolved **once per run** into an `ImportTable` of `ImportTarget { module, function, arity }` cached in `LabelMap` keyed by the owning `Module` address (a different module falls back to the free `import_target`). `GcBif`/`Bif`/`Bif0`/`CallExt` marshal arguments through a fixed-lane `STACK_ARGS = 2` stack array (the dominant arities build an exact-size slice LLVM can SROA); arities >= 3 reuse a per-`Machine` `args_scratch`, so a BIF call allocates **nothing** -- proved by `tests/bif_alloc.rs`. `read_operand` is inlined and ordered hot-first; `Machine::reset` refills the register/FR files in 128-byte chunks. None of this changes semantics: the reference point stays `Machine::step_instruction`.

## The machine: sub-stores, run entries, raise funnel, contexts, introspection (from `../CONTEXT.md`)
Moved verbatim from the crate `CONTEXT.md` (which keeps the contract and a pointer).
`Machine` is the process-state core: the x-register file (`MAX_REGISTER = 1024`, C's `MAX_REG`), the float-register (FR) file (`MAX_FLOAT_REGISTER = 1024`, holding **`Eterm` term slots**), the Erlang stack, the call-frame and exception catch-frame stacks, the `refc_binaries` arena (never cleared by `reset`, host-prunable through `retain_refc_binaries`), the running module's decoded **literal area**, the **module-context registry** (`contexts` + `context` + `entry_literals`), the reduction counter, the **raise-time snapshot** `raise_snapshot`, the latched A.8 gate `trace_on`, and the per-module **label-map cache** (`label_maps`, `src/machine/label_cache.rs`).
- **The label-map cache (`label_cache.rs`).** `build_label_map` is a whole-module value (one traversal per function plus the module's entire import table) and a pure function of the module's immutable content, so `Machine::label_map(&mut self, &Module)` / `Machine::label_map_arc(&mut self, &Arc<Module>)` answer a cached `Arc<LabelMap>` and build only on a miss.
  The cache is a **bounded LRU**: `LabelMaps` holds up to `LABEL_MAP_CAPACITY` (32) `Entry { key, held, map, stamp }` in a `Vec`, `stamp` is a monotone use counter, a hit bumps the entry's stamp **in place** (the `Vec` is never reordered — a move-to-front would memmove up to `capacity` entries on every probe, the regression `local_callee.rs`'s memo documents), and a miss inserts as most-recently-used and, when full, evicts the entry with the smallest stamp (`swap_remove`, so only the eviction path scans).
  An eviction is only a performance event: the evicted module's next probe rebuilds the identical map, never serves a stale one.
  Capacity 32 covers the working set of a compile-shaped run (entry module + the 10–20 distinct in-machine callees of a phase), and retention is bounded by `capacity × live machines` because label maps are per-machine copies.
  The key is **identity, never content**: `Module::checksum()`'s `Digest` + `Module::atom_table`'s `AtomSnapshot` + the module address — the digest covers exactly the chunks the map derives from (`Code` label slots, `AtU8`/`ImpT` imports), the atom snapshot witnesses the namespace the import *names* were resolved against, and the address is what `LabelMap::imports_owner` checks, so a hit is byte-identical to a fresh build and the key may never hold anything runtime-mutable.
  A synthetic image (all-zero checksum) is **never cached**; a second load of one `.beam` and a `Module::clone` each mint a fresh atom-table identity and so are a **miss** (one extra build, never a wrong map); the entries survive `Machine::reset` (which does clear the context registry, so a nested run re-resolves its callees through the `Arc` route — the alternation the LRU exists for), and the `Arc` route holds the handle so its address cannot be recycled (a hit on a bare-route entry installs it).
  Pinned by `src/machine/label_cache_tests.rs` (the `interpreter::label_map_builds()` tally: one build per module, and 2 builds for an A→B→A→B→A run alternation) over the `label_cache.rs` unit tests (hit/alternation at capacity `C` vs 1, LRU eviction, cached-vs-fresh equality over the whole label and import id range); measured in `MEASURED.md`'s label-map-cache section.
- **Run entries (`run.rs`).** `run_export`/`run_label` (non-blocking) and `run_export_outcome`/`run_label_outcome`/`resume_outcome` (blocking-aware; `RunOutcome { Returned(Eterm), Wait { func, pc }, Suspend { func, pc } }`), plus the tier-aware `run_with_tier`/`run_export_outcome_with_tier`/`run_label_outcome_with_tier`/`resume_outcome_with_tier`. `run_export` resolves `function/arity`'s export entry label, seeds `x[0..arity]` from `args`, pushes the bottom frame, runs to `return`, yields `x(0)`; `run_label` runs a raw entry label; a missing export/label is a typed error. The loop is `run_loop`/`run_loop_with_map` (`src/interpreter/dispatch.rs`), thin wrappers over `Machine::drive`.
- **The uniform raise funnel (`catch.rs`).** `throw`/`throw_error`/`throw_with_trace` are the raise/unwind entries the families call; `throw_with_trace` first **rebuilds the raise-time frame snapshot** (the **raising** frame from `current_code`/`current_context`, then every live caller innermost-first, each in its own module context) *before* any unwind, so it survives on **both** the caught and the uncaught path. The symbolic `StackTrace` an **uncaught** raise carries is attached not at the raise site but by the single funnel `Machine::with_uncaught_frames(err, entry_module, func, pc)` as the raise leaves the machine (at the `step_instruction`/`drive` boundary), so **every** raise kind gets the same frames -- only when `raise.stacktrace.is_empty()` and the reason is not an engine diagnostic. A **caught** raise carries no symbolic trace; the same raise-time snapshot is what `Instr::BuildStacktrace` cooks into the `try … catch C:R:S` binding.
- **Module contexts (`context.rs`).** A `call_ext*` the host resolved through `resolve_external_call` executes the callee **in this machine**, so `(func, pc)` are indices into *another* module's table and the machine must remember which. `Contexts` registers/dedups a `ModuleContext { module: Arc<Module>, labels }` by `Arc::ptr_eq` (context `0` is the run's **entry** module, which the machine never owns -- `entry_literals` records its literal area). `switch_context(id)` re-binds `literals` and an unregistered id is the typed `error::unsupported`. `reset` clears the set; a **resume does not** (a suspended cross-module frame must still name its module).
- **Introspection (read-only).** `x(n)`/`get_fr(n)`/`fr_mark()`/`y(k)`/`frame_depth()`/`stack_depth()`/`catch_frame_depth()`/`pending_exception()`/`pending_exception_mut()`/`pending_class()`/`resumption_addresses()`/`resumption_contexts()`/`raise_snapshot()`/`wait_deadline()`/`reductions()`/`reset_reductions()`, plus the execution-position accessors (`current_code`/`entry_call`/`current_function`/`initial_call`/`current_location`/`current_stacktrace`/`current_module`, the engine half of `process_info/2`). Per-file roles: `src/machine/CONTEXT.md`.

## Rooting audit: what this node holds outside the frozen `RootSet`

The complete inventory of `Eterm`-typed state on `Machine`, and its rooting status. Anything
**not** in `x`/`fr`/`stack` is a "loose" term and is only safe if it is parked by
`with_roots` or provably never live across an allocation.
- **The five parked loose terms** (`mod.rs:937-945`, `LOOSE_HEAP_TERMS = 5`):
  `raise_function_clause_args`, `pending_function_clause_args`, `pending_exception`'s reason,
  `pending_supplied_term`, `raise_supplied_term`.
  `with_parked_loose_terms` (`mod.rs:955-981`) pushes them at the stack tail **before** the hand-off,
  hands over the `RootSet`, reads each field back from its slot and `truncate`s —
  unconditionally, so an `Err` out of the closure re-seats too (there is no `?` between the push and
  the truncate). `seat_loose_heap_terms` (`:990-1003`) leaves a field untouched on a `None` read-back.
  Both **term** pairs are in that set; only the `Option<StackTrace>` pairs are metadata and stay out of it.
- **`args_scratch` (`mod.rs:574`) is the one term vector that is not a root** — `mem::take`n out of the
  machine for the whole wide dispatch (`interpreter/bif.rs`) and for `trace_call`
  (`interpreter/trace.rs`), hence invisible to `for_each_root` (documented on that method) and to
  `with_roots`. It cannot dangle because the window is **structurally** collection-free:
  `Machine::dispatch_bif_call` takes `&self` while every admission door (`ensure_heap_full`,
  `alloc_words_rooted`) takes `&mut self` plus this machine's root slices, so the borrow checker forbids
  a collection in the window; and `alloc_words` — the only heap door a BIF can legally reach — never
  collects. Pinned by `tests/bif_args_window.rs` (zero rooted-door crossings across a narrow and a wide
  dispatch, the arguments equal to `x(0..arity)` on both BIF doors, a `test_heap` control proving the
  probe fires) and by the `bif` unit test that the buffer comes back **empty** on the error and the
  success path alike. A BIF-invoked seam that gained a collection point would make the argument words
  have to be rooted — kept in the machine and enumerated, or parked like the loose terms.
- **`class_atoms: [Eterm; 3]`** are atom **immediates** — no rooting needed.
- **Nothing else stores a heap word**: `frames`/`catch_frames` (CP slots are `Continuation::Address{context,func,pc}`
  / `Halt`), `raise_snapshot` and `resumption_addresses`/`resumption_contexts` (`(u32,u32,u32)`),
  `contexts`/`ModuleContext` (`Arc<Module>` + `Arc<LabelMap>`, `context.rs:67-90`),
  `label_maps` (`Digest` + `AtomSnapshot` + address keys, `label_cache.rs`), `local_callees`
  (`CalleeKey`/`CalleeTarget`, `local_callee.rs`), and the literal binding
  (`literals`/`entry_literals` = the load-time area, a separate never-collected heap). `RunOutcome`
  carries only `Eterm` (a returned value) plus `u32` addresses, so a park/resume window holds no term
  outside the registers/stack/FR file.
- **The x/FR files' bounded-refill witnesses** (`registers.rs:290-361`, `used_extent`): `set_x`/`set_fr`
  raise the mark, `x_mut`/`fr_mut` and a root hand-off note the extent they hand out, the native driver
  notes the artifact's `native_write_extent` **before** the block runs (`jit/baseline/mod.rs:696-702`,
  re-declared before an in-block transfer, `jit/baseline/runtime.rs:1708`/`:1728-1737`), and
  `mark_regs_escaped` is the wide fallback. The invariant "every slot >= `max(mark, dirty)` is `NIL`"
  therefore holds, so a stale register cannot survive a `reset`.
- **`refc_binaries` / `writable_payloads`** are pruned only by `retain_refc_binaries`
  (`offheap.rs:126-144`): `Vec::retain` never moves a payload, so a retained `BinRef.val` stays valid,
  and the writable registry is pruned in the same call. The predicate's soundness is the **caller's**
  assertion (a false reject frees a payload a live term names) — the engine cannot verify it.
- **Cross-node hazard, not this node's to fix:** a host that publishes the machine's roots into a
  process `Registers`, collects, and then re-publishes from `Machine::with_roots` again will overwrite
  the relocated words with pre-collection ones (the engine's `with_roots` re-derives from `self.x`/
  `self.stack`, which no collection has touched while the machine is parked). Any such sequence must
  `resync` from the process root set first — see `beam_rt`'s `resync_parked_roots`
  (`src/context/runtime_context.rs:490-523`) and the park/publish/resume protocol in
  `src/runner/CONTEXT.md`. Likewise, a `resync` that declines for a slice-length mismatch halts the
  process's future collections but has already re-seated the machine's loose terms from
  unpublished slots.

## See Also
- `../interpreter/CONTEXT.md` → the per-instruction dispatcher (`step_instruction_with_entry`, also the JIT deopt
  target).
- `../CONTEXT.md` → the engine's binding execution semantics; `../jit/CONTEXT.md` → the native tiers' block ABI.

## `Machine::writable_payloads` — the writable-append registry

A small side list of `(payload base, capacity in bytes)` for the off-heap payloads this machine
allocated **for appending** (`bs_init_writable`'s backing store, C's `BIN_FLAG_WRITABLE` binary),
deposited by `Machine::mark_writable(base, capacity)` and queried by
`Machine::writable_capacity(base) -> Option<usize>` (a linear scan, newest entry preferred; no
BEAM-heap allocation). The capacity recorded is the payload's **real** one, read back from its control
header (`RefcBinary::with_capacity` via `Machine::alloc_refc_binary_with_capacity`, a 4x
`DEFAULT_RESERVATION_FACTOR` reservation), never the `bs_init_writable` hint. The frozen term layout
carries no capacity word, so the capacity has to live beside the machine; keying it per machine also
gives C's `erts_pin_writable_binary` property for free — a payload another process's copy points at is
not in *this* list, so it is never written in place here.

Only `mark_writable` entries are writable: an ordinary binary's payload length is never taken as a
capacity (the roster `Machine::refc_binaries` is *not* consulted). The list is **never cleared by
`Machine::reset`**, for the same reason `refc_binaries` is not — a registry entry must never outlive
the payload it names — and it is pruned only by `Machine::retain_refc_binaries`, which drops every
entry whose base no longer names a live retained payload **in the same call** that releases the
handle, so a lookup can never answer with a freed block's address or a stale capacity.
`Machine::writable_payload_count()` reports how many registrations are live.

**Growth keeps the two books straight.** `Machine::grow_writable_payload(base, wanted_bytes)` is the
one widening door (`beam_alloc::RefcBinary::ensure_capacity`; C's `GROW_PROC_BIN_SIZE` schedule) and
it re-keys (`rekey_writable`) before returning: an `InPlace` outcome moves only the cached capacity, a
`Moved` outcome moves the key to `PayloadRelocation::to` so a stale key can never answer for the freed
block, and both then restore the `len == capacity` handle invariant through
`Machine::cover_writable_capacity` (C's `bin_size`; a relocation copies exactly `len()` bytes, so this
is what keeps the appender's bytes alive). Every refusal — an unregistered payload, a shared handle, the
`max_capacity` cap — folds into `Ok(None)` (the caller promotes to the copy-rebuild form), because each
is a runtime condition with a correct fallback, not an internal fault. Republishing the two
payload-naming words of the accumulator's own `[BinRef][ErlSubBits]` pair (`BinRef.val`,
`ErlSubBits.base_flags`) over a moved base is the *caller's* job (`raw::InPlace::grow`): this
door is safe Rust and never dereferences a term. It is consulted **only on a shortfall**, since it
scans the roster; `Machine::retain_refc_binaries` is not on the append path.

**A payload another party shares can no longer grow.** `beam_alloc`'s mutators refuse unless
`refcount() == 1`, so a shared payload is never reallocated behind another handle's back.
`beam_rt`'s `crates/beam_rt/src/runtime.rs::adopt_escaped_binaries` clones every handle of a finished
machine into its escaped-payload store, so a writable accumulator that **survives a run boundary**
has `refcount() >= 2` and any later append needing growth falls back to the copy-rebuild form
(byte-correct, amortisation lost). An accumulator whose whole loop stays inside one run keeps growing.

Consumer: `families::bitstrings::raw::{writable_append_target, InPlace, payload_aliased_elsewhere}`.

## The off-heap refc-binary retention seam (`offheap.rs`)

`Machine::refc_binaries` is the machine's off-heap payload reservoir; it keeps every refc binary the
machine built alive and address-stable, and `Machine::reset` never clears it (a returned term may still
name a payload after the run that built it). Reclamation is therefore a **host** decision, taken after a
reachability sweep over the process roots, and the seam is two infallible, allocation-free, panic-free
`pub` methods:

- `Machine::off_heap_overhead_bytes() -> usize` — the reservoir's **backing-store** bytes: the sum of
  `beam_alloc::RefcBinary::reserved_bytes()` over the handles. The reservation (not `len()`) is what a
  host charges for, matching `beam_gc::sweep_unreachable_binaries`/`beam_gc::BinVheap` accounting: an
  exactly-sized payload has `len() == capacity() == reserved_bytes()`, while a `bs_init_writable`
  accumulator holds a 4x reservation whose spare room is real memory. The consumer is words-based, so the
  host converts with `bytes / size_of::<beam_term::Eterm>()` (8, a truncating division) and compares
  against `beam_gc::BinVheap::triggered`'s floor, the policy's `min_bin_vheap_words`.
- `Machine::retain_refc_binaries(keep: FnMut(*const u8) -> bool) -> usize` — drops every handle whose
  payload data pointer the predicate rejects and returns the number of handles **released** (a released
  handle frees bytes only when it was the last one, `refcount() == 1`). Retained payloads keep their exact
  `data_ptr`, so compaction never invalidates a live `BinRef.val` (the frozen `beam_term::refc` contract)
  and never moves a block; payloads are always found by `data_ptr` search
  (`grow_writable_payload`/`cover_writable_capacity`), so the compaction is invisible to those doors. The
  same call prunes `writable_payloads` in lockstep (above).

**The predicate is the caller's soundness assertion.** The engine cannot verify it: the reservoir holds
handles, not roots. A false reject frees a payload a live term still names (a dangling `BinRef.val`); a
false accept is merely a leak. The intended use is `beam_gc::for_each_reachable_binary_payload`'s
reachable-pointer set as the predicate, after first **cloning** the handles the host keeps beyond the
machine into its own off-heap table (the clone bumps the refcount, so the payload survives the drop).
Pinned by `offheap.rs`'s unit tests (address stability across a retain, released-reservation accounting, a
shared payload outliving a released handle, writable-registry pruning both ways, the empty no-op).

## The local-call seam's memo (`Machine::local_call_seam`)

All five call sites — the interpreter's three local-call arms (`../interpreter/dispatch.rs:429`/`:475`/`:504`) and the baseline tier's two helpers (`../jit/baseline/call.rs:398`, `../jit/baseline/runtime.rs:1269`) — route through `Machine::local_call_seam` (`../interpreter/dispatch.rs:1149-1212`), which consults the host's `ExecutionContext::resolve_local_call` (`../context.rs:1116`). That method lives in `../interpreter/`, not here; what belongs to **this** node is the memo.
- **Both halves are memoized, per `(module image address, function index)`, for the life of a run.** `Machine::local_callees` (`local_callee.rs`, `CAP = 8`) holds the callee's `(module, function, arity)` identity (`local_callee_identity`, `../interpreter/dispatch.rs:1354`: `module.functions.get(func)` plus two `module.atom_name(..)` — a `Vec` index, no hash, no clone) **and** the host's verdict as a one-byte `Verdict` (`Unnameable`/`Ask`/`Served`/`Declined`) in a parallel array beside the keys, so the hot path is one fixed scan of the occupied prefix plus one byte load: **no** `dyn` call, no `&str` predicate, no re-derivation. `resolve_local_call` is therefore consulted **once per distinct callee per run**, not once per call. `nregs` plays no part in either half (it is the BIF marshalling arity only).
- **Caching the verdict is the faithful choice, not a shortcut.** The host predicate is a pure function of the `(module, function, arity)` triple (for `beam_rt`: `prim_tty`'s stubs plus `beam_bifs`' `zlib` table), and ERTS itself charges **nothing** for an intra-module call — so the per-call consultation the memo replaces was spending a reduction unit (`beam_rt/src/runner/budget.rs:542`, load-bearing slice accounting) the oracle never charges.
- **Three invalidation doors, and invalidation re-asks the host but re-derives nothing** (an identity is a pure function of an immutable module image and cannot go stale). An *installation* is the only thing that can change an answer: `erlang:load_nif/2` **arms** `Machine::local_call_invalidation_pending` (`Cell<bool>`, so the dispatch keeps its `&self` receiver) at the one BIF funnel (`Machine::dispatch_bif_call`, `../interpreter/bif.rs`, gated on `module == "erlang" && function == "load_nif" && arity == 2`, so an ordinary BIF pays one arity compare) and the seam clears every verdict at its **next** local call, because it installs the stub a later local call must reach; `Machine::reset` clears the whole memo *and* the flag, which is also what keeps the address key ABA-free; and the public `Machine::invalidate_local_call_verdicts` (`mod.rs`) is the door for a host whose answer set changes by any other means (its own plugins, a reload, a config toggle) — it clears the flag too, so an explicit invalidation is never followed by a redundant one.
- **A verdict must still never reach compiled code.** `JitBackend::compile(&mut self, module: &Module)` is handed no `ExecutionContext` (`../jit/mod.rs:245`), and the cross-run `CodeCache` is content-keyed and shared across runs, machines and hosts — so the memo is legitimate strictly as a **per-machine, per-run, invalidatable** cache, and nothing the emitter generates may bake a verdict in.

## Measured: the per-`Machine` label-map cache (`label_cache.rs`)
The wall-clock record and the full per-round tables (moved from `MEASURED.md`, because this node owns the cache).
Headline: the cache alone is an interpreter median **0.9861** (sign 14/15) and a baseline-JIT median **0.9662** (sign 12/15) on `cowboy_bstr:to_lower/1`, i.e. ~1.4 % / ~3.4 %; it costs a reproducible ~+20 ns/run (+16 % of the `engine_bench` `setup/empty` floor) on the never-cached synthetic route.

## Measured (per-`Machine` label-map cache)
`src/machine/label_cache.rs` caches up to `LABEL_MAP_CAPACITY` (32) `Arc<LabelMap>`s per `Machine` in a bounded LRU (key = `Module::checksum()`'s `Digest` + `AtomSnapshot` + module address; a hit bumps the entry's stamp in place, a miss inserts as most-recently-used and evicts the smallest stamp; a synthetic all-zero-checksum image is **never** cached), so a `run_*` entry re-entering a module does not rebuild the module-wide `LabelMap`. Mechanism/structural counter: the bullet in `## Measured (interpreter hot-loop micro-optimizations)`; this section is the wall clock, including the cost on the route the cache does not serve.
**Protocol (both rows).** One benchmark process at a time, `taskset -c 3` + `setsid -w`, legs alternated back to back in one session with the visit order rotated per round, `/proc/loadavg` at both ends of every leg. All legs are one `dev`/`test` profile (`opt-level = 1`, deps at 2, `debug = 1`, debug-assertions on); comparing across profiles would invalidate the ratio. Three sides, all built in one session from `git archive` snapshots: `BASE` = `6ef145d8b1` (pre-cache), `CACHE` = `99d3fad27b` (the cache alone), `HEAD` = `8ab438477b` (= `CACHE` + the sibling census-latch commit `d01240e4`), because `HEAD/BASE` alone would credit a per-*entry* change with the latch's per-*instruction* win. The **ratio** is the figure: 1-min load ran **13–89** across these sessions, so absolute ns move by tens of percent.
**Structural counter (deterministic — no timing).** `interpreter::label_map_builds()` (a `cfg(test)` tally), pinned by `machine::label_cache_tests::a_second_run_of_a_real_module_builds_no_label_map` over `erlc`-compiled fixtures: **1** build after a module's first `run_export`, still **1** after a second and third entry (**0** new builds); **2** after a *different* module's run, still 2 on its second entry; **1** after a **second load of the same bytes** (a fresh atom-table identity ⇒ one miss, then hits); **1** for a `Module::clone`; **n** after the *n*-th entry of a **synthetic** module. `machine::label_cache::tests` pins the key/route rules and the LRU policy (11 tests, incl. the recycled-address ABA case, the capacity-`C`-vs-1 alternation counts, the LRU eviction order and the cached-vs-fresh map equality) and `jit::baseline::tests::runs_reuse_the_artifacts_label_map_instead_of_rebuilding_it` asserts `built_by_one_run == 0`. Green: `--lib -- label_cache` **14 passed**, `-- jit::baseline::tests::runs_reuse` **1 passed**.
**Primary row — `--lib -- --ignored --nocapture foreign_bstr_to_lower_decomposition`** (`cowboy_bstr:to_lower/1`, a **real** `erlc` artifact: 5 functions / 129 instructions / **39** distinct labels, so the batch's first `run_*` entry builds the map once). `BENCH_ITERS=50000`, `BENCH_WARMUP=3000`, N=15; 1-min load 53.16–88.92. ns/call is the harness' **min** of 5 sub-runs; `CACHE/BASE`/`HEAD/BASE` are per-round paired ratios:
| round | `BASE` interp | `CACHE` interp | `HEAD` interp | `CACHE/BASE` | `HEAD/BASE` | `BASE` baseline | `CACHE` baseline | `HEAD` baseline | `CACHE/BASE` | `HEAD/BASE` |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 11 569.8 | 11 130.1 | 10 508.8 | 0.962 | 0.908 | 9 154.3 | 8 541.0 | 8 585.4 | 0.933 | 0.938 |
| 2 | 11 460.4 | 11 202.8 | 10 577.5 | 0.978 | 0.923 | 9 338.8 | 8 737.0 | 8 637.4 | 0.936 | 0.925 |
| 3 | 11 275.4 | 11 147.7 | 10 472.8 | 0.989 | 0.929 | 8 960.6 | 8 642.3 | 8 708.7 | 0.965 | 0.972 |
| 4 | 11 206.9 | 11 088.3 | 10 724.9 | 0.989 | 0.957 | 8 961.5 | 8 849.2 | 8 777.6 | 0.988 | 0.980 |
| 5 | 11 650.1 | 11 078.4 | 10 550.0 | 0.951 | 0.906 | 9 088.6 | 8 962.4 | 8 694.1 | 0.986 | 0.957 |
| 6 | 11 498.4 | 11 195.8 | 10 823.1 | 0.974 | 0.941 | 9 483.1 | 8 885.7 | 8 736.7 | 0.937 | 0.921 |
| 7 | 11 786.5 | 11 181.8 | 10 545.4 | 0.949 | 0.895 | 9 180.8 | 8 870.4 | 8 851.2 | 0.966 | 0.964 |
| 8 | 11 274.4 | 11 225.7 | 10 745.5 | 0.996 | 0.953 | 9 240.3 | 9 249.8 | 9 135.6 | 1.001 | 0.989 |
| 9 | 11 223.7 | 11 163.5 | 10 715.0 | 0.995 | 0.955 | 9 067.5 | 8 646.0 | 8 576.0 | 0.954 | 0.946 |
| 10 | 11 377.6 | 11 219.6 | 10 620.0 | 0.986 | 0.933 | 9 225.4 | 8 919.1 | 8 607.6 | 0.967 | 0.933 |
| 11 | 11 493.4 | 11 193.9 | 10 570.0 | 0.974 | 0.920 | 9 291.4 | 8 893.5 | 8 658.2 | 0.957 | 0.932 |
| 12 | 11 434.8 | 11 227.8 | 10 609.5 | 0.982 | 0.928 | 9 522.8 | 8 862.5 | 8 790.9 | 0.931 | 0.923 |
| 13 | 11 419.0 | 11 338.5 | 10 662.0 | 0.993 | 0.934 | 9 289.8 | 9 008.4 | 9 009.6 | 0.970 | 0.970 |
| 14 | 11 196.8 | 11 585.0 | 11 336.5 | 1.035 | 1.013 | 8 985.4 | 9 517.5 | 8 902.3 | 1.059 | 0.991 |
| 15 | 11 675.0 | 11 648.7 | 10 903.7 | 0.998 | 0.934 | 9 189.1 | 9 410.0 | 8 640.7 | 1.024 | 0.940 |
**The cache alone (`CACHE/BASE`), this window:** interpreter median **0.9861** (sign **14/15**), baseline-JIT median **0.9662** (sign **12/15**); paired median absolute saving **158 ns/run** (interp) / **310 ns/run** (baseline). Two further windows agree: `BENCH_ITERS=20000` N=10 → 0.9882 (6/10) / 0.9744 (7/10); `BENCH_ITERS=100000` N=10 (load spiked 14.90→67.11 mid-window) → 0.9814 (8/10) / 0.9533 (10/10). Pooled over the 35 paired rounds: **sign 28/35 interp, 29/35 baseline; median ratio 0.986 / 0.966**; the `median`-of-sub-runs statistic reproduces it (0.9736 / 0.9728). **Deterministic cross-check of the size:** `engine_bench`'s attribution line prices a wide label table at **2.650 ns per extra label per run** (the entry lookup, since the map is built once per machine and module in the warm-up), and 39 labels ⇒ ≈**103 ns/run** for the per-entry build the cache removes — bracketing the **158 ns** / **310 ns** paired median saving on a ~9 000–11 500 ns call.
**`HEAD` vs `CACHE` (the sibling census-latch commit):** interpreter median **0.9449**, sign **15/15**; baseline-JIT 0.9886, 12/15 — so the whole `HEAD/BASE` delta (interp 0.9334, baseline 0.9458) is mostly the latch's per-instruction saving, and this change owns the entry-cost row below.
**Secondary row — the per-run-entry floor (`--test engine_bench -- interpreter_reports_ns_per_instruction`), N=10, 1-min load 44.79–79.93.** `setup/empty` (2 instructions/run) is a **synthetic** `Module::default()` — never cached — so it measures the *overhead* of the cache's never-cached route, not its win: `BASE` 138.9, `CACHE` 159.7, `HEAD` 163.6 ns/run → `CACHE/BASE` median **1.16 (0/10 rounds favour `CACHE`)**, `HEAD/BASE` 1.22 (0/10). **A ~+20 ns/run cost on the synthetic path**, with a mechanism: both routes return `Arc<LabelMap>`, so a never-cached module pays one heap allocation + an atomic inc/dec per consultation (twice per `run_*` entry, ~11 ns each). Real modules (a load-time digest) never take that route: the real-artifact row `real tailcall:even/1 (340)` (27 labels ⇒ ~143 ns of ~26 700 ns = 0.5 %) reads `CACHE/BASE` median **1.013, sign 2/10** — a no-move within noise, as the arithmetic predicts; its `HEAD/BASE` 0.972 (7/10) is the latch again.
**Verdict.** The cache pays where it was designed to: ~**1.4 %** on the interpreter row and ~**3.4 %** on the baseline-JIT row of `cowboy_bstr:to_lower/1` (signs 28/35, 29/35), matching the deterministic label-count prediction within the noise. It does **not** move the per-instruction rows (nothing in the dispatch loop changes), is below noise on a small real module (`tailcall`, 27 labels), and costs a reproducible ~+20 ns/run on the never-cached synthetic path; a borrowed/scratch map instead of an `Arc` would close that.
**Repro.** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib --no-run` and `… -p beam_engine --test engine_bench --no-run`, from `git archive <commit> erts_rust | tar -x -C <dir>` snapshots with `CARGO_TARGET_DIR` per snapshot; then `taskset -c 3 setsid -w <bin> --exact bench::foreign_bstr_to_lower_decomposition --ignored --nocapture` (add `BENCH_ITERS=50000 BENCH_WARMUP=3000`) and `taskset -c 3 setsid -w <bin> --exact interpreter_reports_ns_per_instruction --nocapture`, alternating the three legs inside one session and rotating the order per round. Build provenance alone is ~2 % on this row (prebuilt binaries differ from a fresh `BASE` build by a median **0.977**, 5 paired rounds), which is why every leg above was rebuilt in one session.
