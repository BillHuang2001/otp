# `beam_rt::runtime::roots` — the runtime-level **mutable** root door
## Intent
`../roots.rs` owns `Runtime::for_each_root_family_mut`, the *moving* twin of `Runtime::gather_off_heap_roots` (which only answers liveness): it hands a visitor the writable `Eterm` slots of every host-side root family, one slice at a time, tagged with a `RootFamily`.
`Runtime::for_each_root_mut` is the family-blind form of the same walk — a relocation rewrites a word the same way wherever it is stored — and both are `pub(crate)` and return `beam_common::Result<()>`.
```
for_each_root_mut(&mut self, f: &mut impl FnMut(&mut [Eterm]) -> beam_common::Result<()>)
for_each_root_family_mut(&mut self, f: &mut impl FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>)
```
The door **decides nothing**: it collects nothing, retains nothing, prunes nothing — visiting a word has no effect beyond the writes the visitor makes.
Every per-owner enumerator lives in the owner's own module; `roots/process.rs`'s `ProcessRootsMut` composes `beam_process`'s `gather_roots`/`scatter_roots` pairs plus the mailbox drain-and-rebuild extension trait, so no sibling crate is edited.
`RootFamily` (`pub enum`, capped at crate visibility by `pub(crate) mod roots`) is the tag vocabulary: it names the owner and the container, never the kind of term, its derived `Ord` serves report sorting only, and a store move's `MoveHitOwner::Family(RootFamily, index)` reports through it.
## API Surface
### The traversal order (fixed) — 18 steps, 23 `RootFamily` variants
The order below is `for_each_root_family_mut`'s literal statement order, and every one of the 23 variants is emitted by it (step 4 emits five tags from one loop).

| # | family(ies) | owner seam | `in_value_gather` |
|---|---|---|---|
| 1 | `EvalClosureEnv` | `Runtime::eval_closures` → `Rc::make_mut(body).for_each_root_mut` (clone-on-write; sound only at a non-running site) | yes |
| 2 | `EvalCompiledCaptures` | `Runtime::eval_compiled` → `CompiledClosure::for_each_root_mut` (a `None` cache entry is skipped) | no |
| 3 | `SeqTrace` | `SeqTrace::for_each_root_mut` | yes |
| 4 | `ProcessRegisters` / `ProcessMailbox` / `ProcessDict` / `ProcessSavedArgs` / `ProcessSignals` | per live pid: `ProcessRootsMut::for_each_root_mut` (in `beam_process`'s own canonical collection order) | yes |
| 5 | `MailboxMarker` | `MailboxState::for_each_marker_root_mut`, per `mailboxes` entry | no |
| 6 | `PendingSpawn` | `PendingSpawn::for_each_root_mut` — the argument vector, then `ProcessEntry::Fun`'s fun | yes |
| 7 | `EngineArgs` | `RunnerState::for_each_engine_args_root_mut`, `live`-gated | yes |
| 8 | `ParkedMachine` | `RunnerState::for_each_parked_machine_root_mut` (x registers, Erlang stack, FR prefix), `live`-gated | yes |
| 9 | `RunnerResult` / `RunnerExit` | `RunnerState::for_each_result_root_mut` / `for_each_exit_root_mut` | no |
| 10 | `TimerDelivery` | `Timers::for_each_delivery_root_mut` | yes |
| 11 | `TimerReference` | `Timers::for_each_reference_root_mut` | no |
| 12 | `PersistentTermEntry` | `PersistentTermStore::for_each_root_mut` | no |
| 13 | `TraceMatchSpec` | `TraceState::for_each_match_spec_root_mut` | no |
| 14 | `PortPendingEvent` | `PortRegistry::for_each_pending_event_root_mut` (unreported `pending_events`, held line frames, each port's driver `TermQueue`) | no |
| 15 | `PortData` | `PortRegistry::for_each_port_data_root_mut` (the `erlang:port_set_data/2` payloads) | no |
| 16 | `SelectPayload` | `PortRegistry::for_each_select_root_mut` (every armed select registration's handle + payload) | no |
| 17 | `DistQueuedMessage` | `DistClient::for_each_queued_root_mut`, only when `dist_client` is `Some` | no |
| 18 | `AsyncGcRequestId` | `GcState::for_each_pending_completion_root_mut` | no |
Step 4 is a loop over every live pid, so `processes.pids()` is cloned into a `Vec<ProcessId>` and sorted **first** — that is what lets the loop take `&mut` on the same table, `get_mut` skipping a pid that vanished mid-walk — and the sorted list is also the `live` predicate's source (a binary search, not a `Vec::contains`).
`PendingSpawn` is a tagged step of its own (step 6): its owner tags itself, where every other step goes through the private `tagged` adapter — the one adapter from the family-tagged visitor to the owners' tag-free `FnMut(&mut [Eterm])`.
### `in_value_gather` — the parity predicate and the documented superset
`RootFamily::in_value_gather()` is the machine-checkable record of the table's last column: **11** variants answer `true`, **12** answer `false`.
Every `false` family is a genuine root that the read-only gather deliberately skips, for exactly one of three reasons.
(a) **A second copy of another family's words** — `EvalCompiledCaptures`: a mover must rewrite both copies, while a prune reaches the payload through the closure the copy was made from.
(b) **A stabilized copy read again after the owner died** — `RunnerResult`/`RunnerExit`: the prune releases refc payloads by identity and needs no copy, a mover does because the recorded reason is read long after the process is gone.
(c) **A store the gather leaves to another mechanism** — `MailboxMarker` (the marker and its `#Ref<>` words name no payload), `TimerReference`, `PersistentTermEntry` and `TraceMatchSpec` (their writers stabilize — i.e. flatten — every value), the three port families (their words are stabilized or a driver's own), `DistQueuedMessage` (its words live on the client's own scratch heap) and `AsyncGcRequestId` (an all-immediate reference term).
In every one of those cases a *liveness* answer is already correct without the family, and a *moving* answer is not: the word is stored and will be read again, so its slot must be rewritten when the region it names moves.
### Visitor contract
`f` may **replace any element** of a slice it is handed; that is the whole point of the door.
`f` must **never change a slice's length**, reorder it, or depend on any container's hash/iteration order: an unordered owner buffers its words into a `Vec` and scatters them back positionally, so the order a visitor sees is an implementation detail of the gather.
A slice is **one family of one owner** — a visitor that wants to treat a family as a whole (a word's position within a register triple, say) never sees it glued to another owner's words.
**One owner is borrowed at a time**: no two of the runtime's own borrows are outstanding at once, and each `Rc<RefCell<…>>` store is borrowed for exactly the statement that visits it — `persistent_terms` in its own scoped block, `ports` in one scoped block covering the three port seams — released before the next step, so a visitor that re-enters such a store cannot trip a double borrow.
An `Err` **aborts the walk** and is returned as-is, with the owner of the failing slice writing its (possibly half-rewritten) buffer back **first**: the container and the error leave together, and families already visited keep every write they were given.
### Completeness — whole only at a non-running site
The walk is complete only where nothing is executing: the `-eval` statement boundary and a nested run's epilogue.
During a scheduler pass `runner_state`, `processes` and the scheduler are moved out of the `Runtime` for the pass's duration, so a **running** machine's own root set cannot be enumerated from here — the in-pass park arm publishes it instead (`../context/gc.rs`'s refc-binary prune) — and the coverage argument plus the gap are exactly the ones `../runtime.rs::gather_off_heap_roots` states in its `# Residuals` section.
### `roots/process.rs` — `ProcessRootsMut`
`ProcessRootsMut::for_each_root_mut` visits one process's five families in `beam_process`'s canonical collection order — registers → mailbox → dict → saved_args → signals, the order `Process::run_rooted_collection` gathers those same five in.
It is a **local extension trait** implemented for the foreign `beam_process::Process`, which is why the door lives here and not in the sibling crate: it composes existing public seams (`Registers`/`Dict`/`SavedArgs`/`SignalQueue`'s `gather_roots`/`scatter_roots` pair) and needs no `beam_process` edit.
`GatheredRoots` unifies that pair for the four slice-shaped containers so they share **one** visit and one write-back rule, with the `usize` token opaque here (a register's live-`x` count, a start offset for the other three).
`visit_gathered` gathers into a **fresh** `Vec` per family — which is what makes both token flavours correct at once — hands the whole buffer to `f`, scatters it back with the gather's own token, and skips a family with no words entirely.
The mailbox is the exception: `beam_message::Queue` exposes no mutable view by design (a queued `Message` owns an amortized payload graph — off-heap handles, refc binaries, node entries, and for an oversized payload a whole `Fragments` chain whose words the payload points *into*), so `visit_mailbox` drains the queue into an owned `Vec`, visits each message's `[payload, from, token]`, and rebuilds it through `Message::with_payload` after `set_from`/`set_token`.
`with_payload` is the documented relocation-safe form: it keeps the handles, refc binaries, node entries and fragment chain across, where a fresh `Message` would drop the chain (a use-after-free) and leak the refcounts the message held.
The drain is FIFO and the rebuild re-enqueues in that same order, so the mailbox is never reordered, no message is lost or duplicated, and an in-flight receive scan is unaffected (`QueueCursor` is a positional index that still names the same message).
On the error path the failing message is re-enqueued **unrebuilt**, exactly as drained, plus every message behind it, before the error leaves: each message is all-or-nothing and the queue is whole on **every** exit path.
`Queue::enqueue`'s `debug_assert_enqueue_terms` requires `from`/`token` to be immediates — satisfied by an identity-preserving relocation — and is deliberately not weakened, so a visitor that stores a young heap word there fails loudly instead of leaving a dangling envelope word.
## Test Strategy — `../root_tests.rs`
`../root_tests.rs` is the parity gate: the runtime publishes one root set through **two independent implementations**, and a word the value gather knows about but the mutable walk never visits is a word a mover would leave pointing at the address it evacuated.
Six `#[test]`s over **ten states**, each built fresh by its own builder (`bare`, `booted`, `live-process`, `eval-closure`, `runner-recorded`, `pending-spawn`, `persistent-term`, `trace-match-spec`, `port-data`, `engine-args`); several populate state with no public constructor by writing the runtime's private fields, deliberately — a state is the shape of the root set, not the route that produced it.

| test | what it pins |
|---|---|
| `the_family_walk_matches_the_value_gather_in_every_state` | the value gather's corpus **equals** the multiset union of the `in_value_gather()` families, in every state |
| `the_mutable_walk_covers_the_holders_the_value_gather_omits` | each `false` family is *populated*: `persistent-term`/`PersistentTermEntry`, `trace-match-spec`/`TraceMatchSpec`, `port-data`/`PortData` and `runner-recorded`/`RunnerResult` carry their owner's marker, that marker is **absent** from the value gather, and the whole walk is a strict multiset superset of it |
| `every_process_family_is_visited_and_an_identity_rewrite_changes_nothing` | the five `Process*` families + `TimerDelivery` are all visited (the signal queue holds its two queued signals), and rewriting every word to itself changes neither corpus nor the mailbox's payload order |
| `only_a_stable_engines_arguments_are_visited` | `EngineArgs` is visited for a `RunnerState::spawn_stable` engine (one word, also in the gather) and **not** for the plain `spawn` route — the gate asserted from both sides, with the control engine's process shown live so the gate really is the stability flag |
| `a_visitor_error_writes_the_buffer_back_and_returns_the_error` | an `Err` still writes the half-rewritten buffer back — the rewrite is visible to a second walk, exactly one word changed, the mailbox keeps its count and order |
| `the_walk_is_independent_of_hash_iteration_order` | two runtimes built by opposite insertion orders visit the same families the same number of times with the same multiset |
The comparison is **order-insensitive and multiplicity-aware** — a sorted multiset of raw word values, since `Eterm`'s identity *is* its word — because the two walks deliberately visit families, and a message's envelope words, in different orders.
A **marker** is always the word the *owning container* keeps, read back through the owner's own read API: re-evaluating an expression cannot name a stored root, because `Runtime::eval` copies its result onto the runtime store, so a second evaluation answers a different (structurally equal) address.
A visit-count map (`per_family_counts`) supplies the coverage half a multiset comparison is blind to — a family absent from **both** walks compares equal — which is why the process and `EngineArgs` tests assert counts too.
**Exercised by parity but not populated**, each named in the file's own *States* section: `ParkedMachine` (parking is a runner-side act, `runner/run.rs`, reached only through a slice that really suspends, and no fixture module blocks on a `receive`); `DistQueuedMessage`/`SelectPayload`/`PortPendingEvent` (a live peer / an armed `enif_select` / a real driver write); `EvalCompiledCaptures` (needs a *compiled* spawned closure — `eval_closure_state` registers one but does not spawn it); `SeqTrace`/`MailboxMarker`/`AsyncGcRequestId` (each a live-runtime act).
Run it with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt root_tests`.
## Deliberately not enumerated — the relocation checklist's remainder
Each holder below is reported rather than visited, for a reason that makes a slot impossible or meaningless; the classification a mover needs is (a) a rewritable root, (b) address-keyed — needs **re-keying**, not rewriting, (c) unrelocatable.

| holder | class | why / what a mover must do |
|---|---|---|
| every `RootFamily` slice above | (a) | rewritten in place by the visitor |
| the `-eval` evaluator's live `Env` + Rust-stack locals | (a) for the root `Env`'s values and the driver's `outcome`; (c) for everything else | `eval/scratch.rs::boundary`/`close` hand the root environment's values and the driver's terminal terms over as `MoveSlots` (at the exit the `outcome` fields themselves, through `&mut`, never a copy — a copied slot is rewritten while the word handed back still names the dropped region); a clause-local `Env` clone and the other Rust-stack locals have no `&mut` slot, and the driver runs under `RuntimeContext::with_relocation_exempt` (`eval/mod.rs`) — the Phase-2 blocker |
| `eval_heap`, `eval_heap_retired`, `bif_heaps`, `info_heap` | (c) as holders; the store's own two are the move's **from**/**to** ranges | **word-image** regions: `Runtime::for_each_off_heap_region` visits them and their words must never be dereferenced as terms, so handing them to *this* visitor would be a memory-safety defect; the store move reaches the store's words through `beam_gc`'s relocator |
| `RunnerState::errors` | (c) | a live reason word inside a `BeamError` with no mutable term accessor — needs an upstream `beam_common` seam, so the store move **declines** a range such a word stands in; `RunnerState::for_each_error_live_reason` is the reader the whole-region descent adds explicitly |
| `Runtime::references`, `Timers::refs`, `TimerWakes::deliver`, `SelectWatch::by_fd_handle` | (b) | `Eterm`'s `Hash`/`Eq` are the raw word, so every `Eterm`-keyed map is keyed by **address**; `SelectWatch` is re-keyed by `PortRegistry::rekey_select_handles` at a move's commit, while the other three need none — each key is minted on a **pooled BIF heap**, structurally a different region family from the store |
| `InMachinePosition`, the per-call NIF hosts (`NifHostImpl::envs`, `PendingNifSend`) | (c) | they exist only inside a callback window, which no collection runs in |
| the dist/payload scratch `Heap`s | (c) | regions owned by other crates with no word-range accessor |
The engine arena is **not** on that list: `beam_engine::MinimalContext` exposes `heap_mark`/`heap_mark_matches`/`for_each_heap_region`/`heap_rewind` and `../reclaim.rs` is their production caller.
## Constraints
* **No `unsafe`** and return-based errors only (`#![deny(unsafe_code)]` on the crate): every seam answers `beam_common::Result<()>` and nothing here panics on runtime data.
* **`pub(crate)` throughout**, and `in_value_gather` is the node's one `#[allow(dead_code)]` — a **lint** allowance only (the parity suite is its sole reader today; a compaction consulting the rule is the intended production caller), kept narrow so the rule cannot rot unnoticed.
* **A family is one owner's slot set** — the writes are the visitor's alone, and no step retains, prunes or reorders anything.
* Files stay within the ~1000-line target (`roots.rs` 559, `roots/process.rs` 273, `root_tests.rs` 839).
## Routing Table
* `../roots.rs` → `RootFamily` (the 23 tags + `in_value_gather`), the private `tagged` adapter, `PendingSpawn::for_each_root_mut`, and `Runtime::for_each_root_mut`/`for_each_root_family_mut` (the 18-step fixed-order traversal).
* `./process.rs` → `ProcessRootsMut` (the extension trait), `GatheredRoots` and its four `beam_process` impls, `visit_gathered`, `visit_mailbox` (the drain-and-rebuild).
* `../root_tests.rs` → the parity gate: the multiset/marker comparison over ten states, the stable-engine gate's two sides, the `Err`-write-back case, the order-independence case, and the state builders + helpers.
* Read-only cross-references: `../reclaim.rs` (the whole-region descent's family-blind caller `gather_roots`), `../reclaim/compact.rs` (the store move's corpus — three forwarding sites — and `MoveSlots`/`MoveHitOwner`), `../reclaim/holder.rs` (the family-tagged diagnostic attribution), `../../eval/scratch.rs` (`boundary`/`close`, the caller-side `MoveSlots` hand-off), `../runtime.rs` (`gather_off_heap_roots` and `for_each_off_heap_region`), `../../../beam_process` (`gather_roots`/`scatter_roots`).
