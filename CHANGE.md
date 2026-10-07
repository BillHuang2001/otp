# CHANGE.md — Major Modifications

This file records **major modifications** to the ERTS Rust rewrite: architectural shifts,
major data-structure refactorings, and algorithmic changes. It is a coarse, human-readable log
of *decisions and structural changes*, complementing `git log` (which holds every line-level change).

Format: one entry per major change — date, area, what changed and why. Newest first.

---

## 2026-10-07 — Milestone 2: real OTP boot, full C-ABI symbol surface, CHAMP/GenImmix, live distribution, Stage-2 passes

Scope: the Rust ERTS core advances from "vertical slice" to a runtime with a real boot sequence,
complete NIF/driver symbol tables, an adopted Stage-3 data structure, a wired Stage-4 collector
choice, a live distribution handshake, and a landed Stage-2 compiler-pass framework. Verified:
`cargo build --workspace --all-targets` warning-free, `cargo test --workspace` **5081 tests / 0
failures**, and the differential gate **`total=652 agree=651 mismatch=0 unsupported=1 skipped=0`**
(the single `unsupported` is the documented `erl_eval`-lambda encoding gap).

- **Area:** `beam_ets` (Stage 3 — data structures)
- **Change (data structure refactoring):** `set` tables now default to a **CHAMP** (compressed
  hash-array mapped prefix-tree) open-addressing interior (`champ_interior.rs`) instead of the
  chained-bucket index. 5 hash bits per level; each node compresses children into a
  `datamap`/`nodemap` bitmap pair with inline slots; a full 64-bit `term_hash` match falls back to a
  collision node; removals canonicalize by **inlining** single-entry sub-nodes (pulling a sub-node up
  would misroute every key, since a sub-node is indexed by the *next* 5 hash bits). The chained
  `HashInterior` stays the index for `bag`/`duplicate_bag` and is selectable for `set` via the
  crate-internal `SetInterior`. Measured (`interior_bench.rs`, smallint/atom/binary/tuple keys at
  1e1/1e3/1e5/1e6 entries): CHAMP inserts 2–10× faster, iterates up to 5× faster, stores 2–7× fewer
  index bytes/entry (28–63 vs 105–207 B), reaches lookup-hit parity on binary/tuple keys; it loses
  only on delete (1.6–2.8×) and small-int lookup-miss (up to ~2.1×). No observable `ets:*` change:
  `interior_parity.rs` pins identical traces for both interiors at `keypos` 1 and 2.
- **Area:** `beam_ets` (Stage 3 — concurrency and reclamation)
- **Change:** Tables became `Send + Sync` and shareable across processes via `SharedTable`
  (`Arc<RwLock<Table>>`; a poisoned lock is a typed `unsupported_ets` error, never a panic). One
  `write()` guard makes `insert_new`/`update_counter`/`select_replace` atomic (no lost updates).
  `ets:safe_fixtable/2` fixation is modelled as **deferred reclamation**
  (`Table::{set_fixation,is_fixed,deferred_objects}`): storage for removed objects is held in
  `deferred_frees` while fixed — `delete_all_objects` defers rather than resetting the arena — and
  released at unfix, so `ets:info(T,memory)` is held while fixed and falls at unfix, matching ERTS.
  `TermStore::copy_term` shares exactly the global empty-tuple literal (C `copy.c:130-140`).
- **Area:** `beam_gc` (Stage 4 — GC modernization)
- **Change (algorithmics):** the GenImmix mature-generation option gained a **measurement-derived
  automatic rule** (`selection.rs`): `IMMIX_BREAK_EVEN_WORDS = 8 * BLOCK_WORDS` (2048 words = 8 whole
  Immix blocks). `GcConfig` carries `MatureSelection::{Auto,Explicit(MatureKind)}` (default `Auto`);
  `AnyCollector::from_config` dispatches on the resolved strategy and exposes an inspectable
  `MatureChoice { kind, source, mature_words, break_even_words }`. `GcConfig::default()` still
  resolves to the copying path (the 233-word default mature generation is far below the break-even),
  so default collector behaviour is unchanged. Re-measured by a single-variable sweep
  (`immix_break_even.rs`, a purely long-lived corpus at 0.25…64 whole Immix blocks in two shapes
  under a forced fullsweep): below one whole block Immix pins nothing and is **strictly dominated**
  (5.3× moved volume and footprint, 2.0–2.7× pause); from one block up it pins the whole live set
  (moved words → 0) at a flat ~2.2× pause, and its worst-case benefit/cost ratio plateaus at 8
  blocks. `mature_selection.rs` proves the rule selects Copying below and Immix at/above the
  break-even, that an explicit override wins both ways, and that a threshold-crossing corpus yields
  identical collection-kind sequences and structurally identical reachable-object sets.
- **Area:** `beam_dist` (contract A.5 — distribution)
- **Change (protocol/layering):** the server-side handshake is now **proven live** against a real
  OTP-29 peer (`tests/live_accept.rs`): `'N'` flags/creation exchange, MD5 challenge, `'sok'` +
  `'N'` challenge + `'a'` ack, 2-byte→4-byte framing, and the no-`Complement` rule are byte-verified
  against `erts/emulator/beam/dist.c`. The root cause of the earlier failure was **not** the codec but
  the *advertised node name*: OTP's `dist_util.erl::recv_challenge_new/2` does
  `list_to_existing_atom(Ns)` and compares against the dialled node, so a server advertising a bare
  short name is aborted mid-handshake and the Rust side sees a short read ⇒ `badarg`. Inbound
  control messages now have a semantic seam: `delivery.rs` defines `Delivery`
  (`Send`/`RegSend`/`AliasSend`/`Link`/`Exit`/`UnlinkId`/`Monitor`/`Demonitor`/`MonitorExit`/
  `GroupLeader`/`Other`, field order taken verbatim from `dist.c::erts_dist_command`),
  `Delivery::classify`, and a **defaulted `InboundSink` trait** whose default returns the typed
  `DeliveryOutcome::Unsupported` — so the semantic routing lives in `beam_rt` without adding a
  dependency edge.
- **Area:** `beam_abi` (contracts A.3/A.4 — the frozen C-ABI symbol tables)
- **Change:** the NIF surface is now **176/176 `ErlNifApi` slots populated** and the driver surface
  **108/111**, split one-file-per-family under `src/nif/enif/` and `src/driver/drv/` (each family a
  `pub(super) const fn populate(api)` chained innermost→outermost). Added families: resource types +
  destructor, monitors, dirty schedulers, `enif_send`, `select`, stdout/`*printf`, `enif_alloc_env`,
  `ioq`, map iterators, thread/mutex/cond/rwlock/tsd (over a new in-crate `src/sync/` pthread
  facade), `driver_*` port-queue/PDL/monitor/thread/lifecycle families. The four C **variadic** entry
  points cannot be defined by stable Rust (`rust-lang/rust#44930`) and are provided by a real C shim
  (`csrc/nif_variadic.c`, compiled by `build.rs`), not a stub. New mechanisms: a **thread-local
  current-host stack** (`src/driver/current_host.rs`, `beam_abi::with_current_host`) so port-free
  slots can reach their host (this is exactly C's precondition for `enif_alloc_env`), and
  `ERTS_INVALID_ERL_DRV_PORT`. NIF `.so`s resolve `enif_*` out of the loading binary's `.dynsym`,
  which requires `-Wl,--export-dynamic` (kept in the relevant crates' link args and verified with
  `nm -D`). Fixed along the way: an `enif_map_iterator_destroy` null-deref (a missing
  `is_term_word` guard), the `enif_make_resource`/`enif_get_resource` round trip (the term was minted
  through a defaulted host hook and raised `badarg`; it is now minted in-crate as the frozen
  internal-reference block), and `enif_is_process_alive` on self (now answered from `self_pid`
  first, as C's `erts_proc_lookup` does).
- **Area:** `beam_io` (contract A.4 — driver host services)
- **Change:** a process-global driver registry `DriverTable` (`src/abi/table.rs`, the C's one
  `driver_list`): `add_entry` (runs `entry.init` first; non-zero ⇒ the `driver_init_failed` atom and
  nothing recorded), `add_loaded`, `remove_entry` (safe — pointer comparison only, never
  dereferenced), `lock_driver`, `lookup`, and `create_port(&mut PortRegistry, &mut Poller, …)` which
  releases the mutex before running `start` and keeps the `ErlDrvPort` handle stable for the port's
  whole life via `DriverPort::from_library`. `beam_io → beam_abi` (+ `beam_alloc`) is a legal
  wave-5→wave-0 edge.
- **Area:** `beam_bifs`
- **Change:** a real **`bif.tab` surface** (`src/bif_tab.rs`) parsing the actual grammar of
  `erts/emulator/beam/bif.tab` (`bif`/`ubif`/`hbif` + `module:name/arity` + an ignored trailing
  `C-name`, whole-line `#` comments, quoted operator names unquoted to the registry key, and
  `name/lo-hi` arity-range expansion). It is **validation only** — BIF dispatch is unchanged (the
  registry is a hand-written table of `fn` pointers). Measured census: declared **556** (`bif` 476 /
  `ubif` 73 / `hbif` 7), implemented **241**, missing **315** (enumerated, never fabricated);
  registry 320 = 241 declared + 79 stdlib fast paths. Also fixed the `lists:usort/2`
  **wrong-result** bug (the ascending split's `Y < Z` branch omitted `x = y`, duplicating an element:
  `usort(A =< B, [1,2,3])` returned `[1,1,3]`) and added an additive `LocalFunIdentity` seam so a
  compiled module's `make_fun3` closure encodes to a byte-exact `NEW_FUN_EXT`.
- **Area:** `beam_loader` + `beam_engine` (Stage 1)
- **Change:** the loader's coverage is now measured rather than estimated (`Module::records()`,
  `Recs` chunk exposure, literal-area decode incl. compressed form / `EXPORT_EXT` / `NEW_FUN_EXT` /
  bignums / >32-key maps): **1366/1366 installed `.beam` across 34 applications load with 0
  failures**, and the residual `Instr::Generic` census over 261 modules / 793 319 instructions is
  **empty (0 distinct opcodes)** — the loader lowers every compiler-emitted opcode 1:1. The
  "blocked" `Instr` variants are mostly synthetic-probe artifacts; the genuine residue is host-seam
  work owned by `beam_rt`/`beam_bifs`. New read-only execution-position accessors
  (`src/machine/location.rs`): `current_code`, `entry_call`, `current_function`, `initial_call`,
  `current_location`, `current_stacktrace` — written at the single choke point shared by the
  interpreter and the native tiers' deopt helper, so the address is exact at every suspension.
- **Area:** `beam_engine` (Stage 1 — the execution-engine seam)
- **Change (architecture):** the JIT behind the typed-IR seam gained a block register allocator
  (r12–r15 + rbp), a cross-run `CodeCache` keyed by `ModuleKey`, direct block linking, and
  comparison inlining (measured 2.4×–14.9× on the affected blocks). The engine's largest source file
  was split from 75513 to under 911 lines across 7 responsibility modules. Coverage census: `Instr`
  106 = 49 executed + 57 typed-blocked + **0 unclaimed** (`unclaimed == 0` asserted);
  `TestKind` 25/25. Correctness-first is preserved: the JIT deopts into `jit_step_one` rather than
  ever producing a wrong result.
- **Area:** `beam_rt` (contracts A.6/A.7/A.8 — the runtime binary)
- **Change (A.7 — boot):** `Runtime::boot()` no longer *names* the preloaded modules, it **loads
  them**: `src/boot/preloaded.rs` loads all 22 C-preloaded `.beam`s from the erts app `ebin`, and
  `src/boot/bootscript.rs` is a real `start.boot` reader + executor (`BootScript`/`Instruction` over
  `beam_codec::decode_term`) with `boot_from_script`, `load_code_module`, `add_code_path`,
  `register_boot_process`. `erts_internal` grew from 1 to 13 names and `prim_file` from 0 to 11
  entries, each pinned against the oracle. The real `start.boot` executes directives 1–15 and
  honestly stops at instruction 16 `{apply,{application,load,[…]}}` (no `application_controller`
  yet) with a typed error; a synthetic `preLoaded`+`module`+`apply` script completes end-to-end.
- **Change (A.6 — CLI/configuration):** the flag surface went from ~12 to full coverage
  (`+Q`/`+P`/`+K`/`+sbt`/`+A`/`+hms`/`+hmbs`/`+hmax*`/`+M*`/`+MM*`/`+MB*`, `-config`, `-setcookie`,
  `-boot`, `-emu_flavor`), and the real `ERL_*FLAGS` precedence chain is implemented
  (`[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [cmdline] [ERL_FLAGS] [ERL_ZFLAGS]`) with the C's
  `build_args_from_string` tokenizer. `+Q N` now **drives** `system_info(port_limit)`
  (`min(next_power_of_two(request), 134217727)`, default 65536) instead of pinning a constant, and
  `ERL_MAX_PORTS` is routed through the same channel.
- **Change (A.8 — introspection):** `system_info/1` went from 16 to 63 oracle-accepted keys and
  `process_info/1,2` from 0 to 27 items, with oracle-pinned value **and shape** equality for 38
  comparable keys; the family was split into `src/info/{system,limits,schedulers,memory,unsupported}.rs`.
- **Change (execution):** the `run` action now drives the **full scheduler**
  (`Runtime::run_scheduled`, bounded `RUN_DEADLINE`/`RUN_STEP_SLICES`) — run queue + ports +
  mailboxes + timers — so a blocking workload (an A.4 driver/port echo round-trip) completes instead
  of failing with `unsupported: … blocked`; the runner records the original `BeamError` per pid so
  the raised class and the exit-3 capability-gap contract are preserved. The scheduler/run-pass
  family was extracted from `src/runtime.rs` into `src/runtime/scheduler.rs` (runtime.rs 2250 →
  1241 lines).
- **Area:** `beam_rt` (distribution) + `beam_message`
- **Change:** the node now advertises the **full `node@host`** on the wire (a bare short advertised
  name is refused by a real peer), so a real `erl`'s `net_kernel:connect_node/1` returns `true`
  against a running `beam_rt` binary (proven in-process and through the real binary).
- **Change (correctness, `beam_message`):** `src/layout.rs` no longer sizes/walks fun blocks with the
  legacy `ERL_FUN_SIZE` (env at offset 2); it derives both from the frozen `beam_term::fun` layout
  (`env[0]` at `1 + desc_words(kind)`, block = `fun_words(kind, num_free)`), matching what
  `beam_gc` and `beam_term::size` already do. The legacy math made `copy_struct` mis-size a closure
  crossing a copy boundary (verified to abort with a null dereference in `beam_term/src/list.rs`).
- **Area:** `lib/compiler` (Stage 2 — compiler optimization)
- **Change (framework):** a per-pass **time budget + watchdog + eligibility/ordering hook**
  (`src/beam_ssa_opt_budget.erl`, options `ssa_opt_budget` / `ssa_opt_watchdog` / `ssa_opt_gates`).
  With the options off the pass funs are literally unwrapped, so default compilation is
  byte-identical. Budget truncation skips a **monotone suffix** of optional passes (less optimized,
  equally correct); a hard per-pass watchdog kills a runaway pass in a monitored child and **aborts**
  rather than silently skipping (silent skipping was measured unsound — a non-deterministic pass
  subset). Measured on `lib/stdlib/src/lists.erl`: default median 305 ms, `{ssa_opt_budget,5}`
  median 243 ms.
- **Change (passes):** `beam_ssa_loop_opt.erl` gained strength reduction (`X*2^n`→`bsl n`, `X*2`→
  `X+X`, `X div 2^n`→`bsr n` under integrality/non-negativity proofs) and induction-variable
  canonicalisation; `beam_ssa_fus.erl` gained multi-key map-key test-chain elimination (the
  redundant `has_map_fields`/`is_map_key` chain is collapsed and the multi-pair `get_map_elements`
  is still synthesised by `beam_block:opt_maps/2`); `beam_ssa_fbip.erl` was audited. All passes stay
  **opt-in/OFF** because global equivalence was not proven. FBIP widening to maps was investigated
  and found **infeasible compiler-only**: no map-update instruction in `ops.tab`/`genop.tab` (emu or
  JIT) carries a `copy|reuse|inplace` hint, `beam_ssa_codegen.erl:2398` emits copying
  `put_map_assoc` unconditionally, and `erl_map.c` always allocates — so the emitted map assembly is
  byte-identical with and without `+fbip_opt` (control: the same build still rewrites `update_record`
  `copy`→`inplace`, proving the flag path is live). It is an emulator+runtime feature, not a
  compiler-only widening, and is recorded as such rather than faked.
- **Area:** `tools/differential` (the compatibility gate)
- **Change:** the corpus grew 431 → **652** cases with a new A.4 **runtime** group (`driver_c.rs`,
  a real C driver `.so` driven through its port lifecycle — A.4 previously had no runtime case at
  all), A.3 resource/ioq/sync cases, A.6 flag/env cases, and A.8 `system_info`/`process_info` cases,
  all with oracle-derived goldens. A new `CaseKind::EvalEnv` + `runtime::run_with_env` expresses
  environment-variable cases **without** touching the existing `CaseKind::Eval` literals, and pins
  the `ERL_*FLAGS` precedence chain (including a single-valued switch proving a later source
  overrides an earlier one).
- **Area:** `tools/bench`
- **Change:** a new `foreign` workload measures the **Rust runtime** on real target-project code for
  the first time (Cowboy `cowboy_bstr` and EMQX `emqx_base62`, compiled once with the system `erlc`
  and run under both runtimes). Measured (startup-corrected ns/call, `--quick`): `to_lower` sys
  437.79 vs Rust 268 618.72; `emqx_base62:encode` sys 175.74 vs Rust 2 092 849.08; startup
  comparable (~130–160 ms both) and peak RSS 70.9 MB (sys) vs 28.8 MB (Rust). The gap is execution
  (today a tree-walking interpreter vs the JITed BEAM), reported honestly as a real result.
  `khepri` runs via `$BENCH_TRY_NIX` (`rebar3 eunit` 77 340 ms, peak RSS 357.6 MB); `plb` and `emqx`
  remain honest skips with the true reasons recorded.
- **Area:** legacy C removal
- **Change:** three genuinely dead C files removed (`git rm`, with `rg` evidence over every
  `Makefile*`/`*.mk`/`*.in`/`*.tab`/`*.am`/`configure*`/`otp_build`/`*.sh`/`*.c`/`*.h`):
  `erts/emulator/drivers/unix/multi_drv.c`, `erts/emulator/drivers/unix/sig_drv.c`,
  `erts/emulator/drivers/vxworks/vxworks_resolv.c`. `bin_drv.c` was **not** removed: the same sweep
  found live references in `erl_nif.c`, `dist.c`, `io.c`, `erl_bif_info.c`, and `erl_binary.h`, so
  the earlier "dead file" claim was wrong for it and the file stays.

---

## 2026-09-01 — Milestone 1: crate tree, frozen term layout, typed-IR engine, ETS/GC/messaging interiors

- **Area:** `erts_rust/crates/*`, `erts_rust/tools/*`
- **Change (foundation):** Froze the **term representation** in `beam_term` as a bit-exact port of
  `erts/emulator/beam/erl_term.h`: 2-bit primary tags, 60-bit small integers, 6-bit-shifted atoms,
  header-less 2-word cons cells, boxed sub-tags, and `TAG_LITERAL_PTR` literal pointers. `Eterm` is a
  `#[repr(transparent)]` newtype over `usize`, ABI-identical to a NIF's `ERL_NIF_TERM`. All term
  construction/inspection is centralized here (single source of truth for every other crate).
- **Area:** `beam_ets` (Stage 3 data structures)
- **Change (data structures):** ETS keeps the immutable **observable** `ets:*` semantics (key
  equality/ordering, duplicate-key winners, `size`, lookup/select results) while the **interior
  layouts are opened up** (Part II §B/§D). `set`/`bag` use a bucket hash index; `ordered_set` uses a
  key-sorted interior — the legacy **AVL tree is replaced** by a sorted structure in M1 and is slated
  to become a **K-ary B+-tree** (Part II §B.3). Object storage is a never-relocating table-owned bump
  arena (the copy boundary); deletion does not yet reclaim blocks (MSO planned).
- **Area:** `beam_gc` (Stage 4)
- **Change (algorithmics):** Implemented the per-process **generational semi-space Cheney copying**
  collector, preserving the **barrier-free** generational invariant (a free consequence of term
  immutability), with a refcounted **MSO** off-heap sweep. MMTk/Boehm are rejected; GenImmix is a
  measurement-gated future option (Part II §D).
- **Area:** `beam_message`
- **Change (algorithmics):** Implemented the `copy_struct` copy engine + send/receive envelope — the
  per-process-heap copy boundary (Part II §D). Messaging across heaps remains copy-based by design.
- **Area:** `beam_loader` + `beam_engine` (Stage 1 — the execution engine)
- **Change (architecture):** The loader lowers `.beam` to a **typed, block-structured IR**; backends
  consume only the IR. This is the pluggable **execution-engine seam** (Part II §A/§F, "Ruby-YJIT
  shaped"). A **direct-threaded interpreter** implements an honest generic-opcode subset; the
  **BeamAsm-style baseline JIT** and the **optional Cranelift hot tier** sit behind the same seam and
  are measurement-gated / off the critical path. Root-set contract kept: `x_regs[0..live)` + the whole
  stack, **no frame liveness maps**.
- **Area:** `beam_dist`, `beam_abi`
- **Change:** Distribution protocol v6 (`DFLAG_*`/`DOP_*`, handshake, control framing, `epmd` client)
  and the layout-frozen NIF (`enif_*`) / driver (`driver_*`/`erl_drv_*`) C-ABI surfaces, with
  static size/offset assertions and `Eterm ↔ ERL_NIF_TERM` transmutes.
- **Area:** `beam_rt`
- **Change:** The runtime binary (boot, CLI flags, `system_info`) wires the engine + BIFs + process
  context and executes a real `.beam` end-to-end (the vertical slice): `beam_rt run slice.beam
  slice:add/2 2 3` → `5`, `slice:fib/1 10` → `55`.
- **Area:** build system
- **Change:** Developer-iteration build configuration: `mold`/`lld` linker, `opt-level=1` dev profile
  with dependencies at `opt-level=2`, thin-LTO release, and a Nix flake pinning the toolchain plus the
  oracle/target runtimes. `scripts/cargo.sh` selects a writable `CARGO_HOME` for read-only-`$HOME`
  sandboxes.
- **Area:** `tools/differential`, `tools/bench`
- **Change:** The differential compatibility gate defines a **capability-gap contract** (runtime exits
  `3` with `beam_rt: unsupported:` for an unimplemented opcode/BIF/`-eval` form) distinct from a real
  runtime error, so an incapable runtime is reported `unsupported` (honest), never `mismatch`; new
  module-run cases give real oracle-vs-Rust agreement. The benchmark driver gains a **read-only
  target adapter** (build in a `$TMPDIR` copy of a read-only foreign repo) and a **dependency-free
  HTTP/1.1 load generator**, so it measures Cowboy, the Elixir/`erlc` compilers, and the system-ERTS
  baseline with real numbers.

## 2026-09-01 — Bootstrap: Rust ERTS rewrite workspace established

- **Area:** repository root / `erts_rust/`
- **Change:** Introduced a new Cargo workspace, `erts_rust/`, as the from-scratch Rust
  reimplementation of ERTS (the BEAM runtime). It supersedes the legacy C/C++ emulator for the
  new build path; the legacy tree under `erts/` is retained as the **differential-test oracle**
  and the `FLAVOR=emu` bootstrap fallback, to be **removed module-by-module** as Rust parity is
  proven by the differential harness.
- **Why:** The rewrite targets full drop-in compatibility with Erlang/OTP (`30.0-rc0` / `erts-17.1`),
  higher robustness (safe periphery), a modern codebase, and improved speed + memory. The staged
  decomposition (Stage 0–6) comes from `ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and
  `ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II).
- **Structure:** 15 library/binary crates + 2 tooling crates, decomposed along the BEAM subsystem
  seams documented in Part II §A/§H. Cross-crate contracts (frozen term representation, error
  strategy, IR seam, root-set contract, differential-test gate) are fixed at the workspace level in
  `erts_rust/CONTEXT.md`.
- **Tooling:** Nix flake (`erts_rust/flake.nix`) pins the dev environment; `.cargo/config.toml`
  configures a fast linker and fast dev build profiles for iteration speed.
