# `boot::drive` — the drive's loops, the processes a boot leaves, and what a boot allocates

## Intent
`src/boot/drive.rs` is where a boot directive's own code runs: `start_kernel_process` spawns a `{kernelProcess, …}` MFA in a short-lived process and unlinks the server it registered, `apply_boot_call` runs an `{apply, …}` in band on the runtime's root identity, and `drive_scheduler` steps the scheduler in bounded bursts.
This node owns the drive's *observable consequences* — the unlink that keeps a directive's server alive, the one live-but-idle port directive 18 starts, the process inventory a completed boot leaves, which directives reach module code, and what each costs in memory.
The directive sequence and the completion contract are the parent node's ([`../CONTEXT.md`](../CONTEXT.md)); the exposed signatures are its `## API Surface`.

## Surviving a `{kernelProcess, …}`'s directive exit (the unlink)
This runtime runs a `{kernelProcess, …}` MFA in a **short-lived** process that exits when the MFA
returns, whereas a real `init` runs it in the **one** long-lived `init` process
(`erts/preloaded/src/init.erl:1520`; its `start_in_kernel/5` is **not exported** from `init.beam`, so it
cannot be called). The kernel servers that matter (`logger_server:start_link/0` via `gen_server`, and
`application_controller:start/1`'s `spawn_link`ed `init/2`) are `proc_lib`/`gen_server` children of that
process: such a server **traps exits** and terminates when its parent exits with `normal` (`proc_lib`'s
parent-exit semantics). Left linked, the short-lived directive's ordinary return tears its own server
down and releases the name — which is why a `gen_server:call` to it then fails with `noproc`.
`start_kernel_process` therefore **unlinks** the server from the directive as soon as `whereis(Name)`
resolves, while the directive is still alive (it is blocked in `receive` until the server acks): an
unlink beats the exit because the server's `{'EXIT', _, normal}` is never produced, and a `{'EXIT',…}`
already queued cannot be undone by a later unlink — hence `KERNEL_BURST_SLICES = 1` (the burst advances
one slice, then re-checks `whereis` at the loop top, landing in the window where the server has
registered but the directive has not resumed). A directive that has **already** terminated
(`heart`'s `start/0` returns after it `spawn`s a process it does **not** link) is skipped, not an error;
`unlink(a, a)` for a self-registering directive is the process table's documented no-op.

## Kernel start performs a real native hostname lookup and leaves one live-but-idle port
Directive **18** (`{apply,{application,start_boot,[{application,kernel,Props},permanent]}}`) starts the
`kernel` application, so `kernel_sup` starts its `inet_db` child (`lib/kernel/src/kernel.erl:211-216`):
`inet_db:start_link/0` runs `inet_config:init/0` before it answers (`lib/kernel/src/inet_db.erl:121-123`),
which
- opens a real UDP socket — `set_hostname/0` → `inet_udp:open(0,[])`
  (`lib/kernel/src/inet_config.erl:223-231`) — i.e. a `{spawn_driver,"udp_inet"}` port driven through the
  numeric `INET_REQ_OPEN`/`INET_REQ_GETHOSTNAME` control verbs, and closes it again after
  `prim_inet:gethostname/1` (`lib/kernel/src/inet.erl:2487`, `erts/preloaded/src/prim_inet.erl:1435-1436`);
- then, when that short hostname carries no dot (so `inet_db:res_option(domain)` is `""`), `init/0`
  takes the Linux branch (`inet_config.erl:110-132`) and calls `inet:gethostbyname/1` → the `[native]`
  lookup backend → `inet_gethost_native:getit/2` (`lib/kernel/src/inet_gethost_native.erl:514-578`),
  which starts the helper **port program** `<bindir>/erts-17.1/bin/inet_gethost` with
  `open_port({spawn_executable, Prog}, [overlapped_io,{args,["4"]},{packet,4},eof,binary])`
  (`:413-429`) and sends the framed request via `port_command`; the answer arrives as
  `{Port,{data,<<RID:32,Bin>>}}` in the module's `server_init/2` loop.
  A hostname that *does* carry a dot sets a non-empty `domain`, so the lookup — the helper process and
  `inet_gethost_native.beam` with it — does not happen at all; the same holds when `inet_udp:open/2`
  answers an error tuple, because `set_hostname/0` then lands on its `_` arm (`:229-230`) and the hostname
  becomes `"nohost"` with domain `"nodomain"`.
- Consequence for the boot drive: **that child is never reaped and never closed**
  (`inet_gethost_native` passes no `exit_status`, so no round polls or reaps it — see
  [`../../port/CONTEXT.md`](../../port/CONTEXT.md)), so from directive 18 on the runtime holds a live, idle
  port and `PortRegistry::has_nothing_to_poll` never answers `true` again.
  A pass that runs **zero** slices polls its ports once (`../../runtime/scheduler.rs` →
  `../../port.rs::pump`), and the drive is the **one** caller whose poll budget is gated on a port owing a
  reply: under `TimerWait::ImminentWithin` a pass with no port holding unanswered bytes
  (`PortRegistry::awaits_port_reply`, the `output > input` rule of
  [`../../port/CONTEXT.md`](../../port/CONTEXT.md) *Measured*) hands the poll a **zero** timeout, so the idle
  helper costs no wall time; the `run`/`-eval` routes keep the blocking poll, because a child there may
  speak unprompted.
  `apply_boot_call` ends **each** `RunOutcome::Wait` with a terminating `pump_scheduler_boot == 0` step
  (`drive.rs:397-412` → [`drive_scheduler`] at `:299-314`), and directives 18/19/20 make 2 + 2 + 1 waits
  (`application_controller:start_boot_application/2` = `application:load` + `start_application`,
  `lib/kernel/src/application_controller.erl:271-284`; directive 20's `c:erlangrc/0` file-server call),
  so those five steps are where an ungated poll would cost ≈ 125 ms of pure latency after the lookup.
  The reply's own wake is untouched: until the helper answers, its request bytes are outstanding
  (`output > input`), so the poll that delivers `{Port,{data,<<RID:32,Bin>>}}` still blocks as it must
  — and a **zero-slice step that still has such an obligation is not the drive's stop condition**
  (`drive_scheduler` keeps stepping, bounded only by `DRIVE_DEADLINE`), because the answer arrives in
  a later round and not inside the one poll slice the step just spent.
  The `-eval` action itself adds no pass (`erlang:halt` returns the halt through the native floor, no
  trailing pump).
  Both sides of the comparison and the reproduction recipe are in
  [`../../PERF.md`](../../../PERF.md) *The boot path's one live-but-idle port, and the poll budget*.

## Process inventory after `{progress,started}` — what a boot leaves alive
What a booted `-eval`/`-s`/`-run` action runs *with* — the question every "what is live at `halt()`" probe asks.
A plain non-distributed `-noshell` run leaves **≈33–38 live processes** at the instant an action's expression runs (on this host, whose hostname has no dot: **38**); every figure here is a static reading of the child-spec code.
- **The inventory.** **4 runtime stand-ins** (`../../../CONTEXT.md` § *Process-creation sites*: `init`, the root pid, `erl_prim_loader`, and the native `file_server_2` — unregistered on this route, `../../runtime.rs:1427` → `../../native/file_server.rs:264-273`) **+ `application_controller`** (directive 14) **+ `logger_server`** (directive 13) **+ the kernel application's `application_master`** (`lib/kernel/src/application_master.erl:47-49,116-150`) **+ the `kernel_sup` tree** (directive 18) **+ the action process** (`../../runtime.rs:5189`).
- **The `kernel_sup` tree's core is 25 processes**: `kernel_sup` + `code_server` + `standard_error`'s supervisor/device/`prim_tty` writer + `file_server_2` + `inet_db` + `rex` + `global_name_server`'s locker and registrar + `global_group` + `global_group_check` + `erl_signal_server` + `user_sup`'s bridge + `user_drv` + its `prim_tty` reader and writer + the `user` io server + `logger_sup`'s watcher and proxy + `kernel_config` + `kernel_refc` + `kernel_safe_sup` (`lib/kernel/src/kernel.erl:95-170`, `:210-226`).
- **Up to 5 more are run-time-conditional**: `rex`'s `nodes_observer` (`lib/kernel/src/rpc.erl:163,376-393`, alive only if its `ets:new`/`persistent_term`/`monitor_nodes(true)` all succeed), `logger`'s default handler (`logger_std_h`) and its `file_ctrl` (`lib/kernel/src/logger_std_h.erl:280-290`), and `inet_gethost_native`'s supervisor bridge + server (`lib/kernel/src/inet_gethost_native.erl:152-156,198-208`, reached only when the short hostname carries no dot — `inet_config.erl:110-132`).
- **`-noshell` does not remove the user tree** — `user_sup`'s `get_user/1` maps the flag to `{user_drv, start, [#{initial_shell => noshell, input => cooked}]}` (`user_sup.erl:126-131`), so `user_drv`, its `prim_tty` reader/writer and the `user` io server are live exactly as under a shell.
- **A completed boot is what proves the tree is real.** Directive 18 starts `kernel_sup` from `lib/kernel/src/kernel.erl:91-227` with `one_for_all`/`intensity => 0`, so any permanent/transient child's start failure aborts kernel (and the boot); `standard_error`/`user` are the two temporary ones, confirmed live by `boot/io_servers_registered_under_boot_flag` `{true,true}`.
- **`application_controller` (directive 14), the `kernel`/`stdlib` application masters and `logger` (= the booted `logger_server`) are live**, and the one live **port** is the `inet_gethost` helper (the section above).
- **Not started**: `timer_server`, `erl_compile_server`, `disk_log_server`, `pg`, `boot_server` — `kernel.app.src` declares no `start_timer`/`start_compile_server`/`start_disk_log`/`start_pg`/`start_boot_server` key, so those `{ok, true}` arms in `kernel.erl:296-363` never fire — and no `peer_supervision` (`peer:supervision_child_spec/0` needs `-user peer`, `lib/stdlib/src/peer.erl:1247-1257`); `on_load` started and exited normally after its ack (`kernel.erl:228-234`).
- **Every process a directive created directly is dead**: the three `{kernelProcess, …}` directive processes (`../drive.rs:212`, ended by `:261-263`), `heart` (`heart.erl:157-196`, `start/0` returns `ignore`), `application_master`'s one-shot "process X" (`application_master.erl:159-162` → `:231`), and `{apply, …}` runs on the runtime's own process (`../drive.rs:350`, no spawn).
- **The executor is not a process**: `cli.rs::boot_then` → `run_boot_script` → `Runtime::boot_from_script` → `bootscript::execute_reporting` runs on the caller's main-thread Rust stack (`../../cli.rs:2011-2026`, `../../cli.rs:2051-2054`, `../../runtime.rs:3961-3978`, `../bootscript.rs:356-389`); an `{apply, …}` directive runs **in band** on the runtime's root identity (`self_pid`, `<0.1.0>`), because `apply_boot_call` builds `RuntimeContext::new(runtime)`, whose `mailbox_pid` is `Runtime::self_pid` (`../drive.rs:376,416,437`).
- **The `-eval` body runs on a fresh, unregistered action child**, not on the root (`Runtime::as_action_process`, `../../runtime.rs:5102-5112,5184-5194`).
A plain non-distributed `-noshell` run leaves **≈33–38 live processes** at the instant an action's expression runs (on this host, whose hostname has no dot: **38**); every number below is a static reading of the child-spec code, since no built `beam_rt` exists to probe.
**4 runtime stand-ins** (`../../CONTEXT.md` § *Process-creation sites*: `init`, the root pid, `erl_prim_loader`, and the native `file_server_2` — unregistered on this route, `../../runtime.rs:1427` → `../../native/file_server.rs:264-273`) **+ `application_controller`** (directive 14) **+ `logger_server`** (directive 13) **+ the kernel application's `application_master`** (`lib/kernel/src/application_master.erl:47-49,116-150`) **+ the `kernel_sup` tree** (directive 18) **+ the action process** (`../../runtime.rs:5189`).
The `kernel_sup` tree's core is **25** processes: `kernel_sup` + `code_server` + `standard_error`'s supervisor/device/`prim_tty` writer + `file_server_2` + `inet_db` + `rex` + `global_name_server`'s locker and registrar + `global_group` + `global_group_check` + `erl_signal_server` + `user_sup`'s bridge + `user_drv` + its `prim_tty` reader and writer + the `user` io server + `logger_sup`'s watcher and proxy + `kernel_config` + `kernel_refc` + `kernel_safe_sup` (`lib/kernel/src/kernel.erl:95-170`, `:210-226`).
Up to **5 more are run-time-conditional**: `rex`'s `nodes_observer` (`lib/kernel/src/rpc.erl:163,376-393`, alive only if its `ets:new`/`persistent_term`/`monitor_nodes(true)` all succeed), `logger`'s default handler (`logger_std_h`) and its `file_ctrl` (`lib/kernel/src/logger_std_h.erl:280-290`), and `inet_gethost_native`'s supervisor bridge + server (`lib/kernel/src/inet_gethost_native.erl:152-156,198-208`, reached only when the short hostname carries no dot — `inet_config.erl:110-132`).
**`-noshell` does not remove the user tree** — `user_sup`'s `get_user/1` maps the flag to `{user_drv, start, [#{initial_shell => noshell, input => cooked}]}` (`user_sup.erl:126-131`), so `user_drv`, its `prim_tty` reader/writer and the `user` io server are live exactly as under a shell.
- **Per-process cost, for sizing the inventory** (`beam_process/CONTEXT.md`, `../../runner/CONTEXT.md`): one engine `Machine` ~26 KiB (`Machine::new` at spawn, `runner/entry.rs:514`) + the inline `Process` PCB ≈9.3 KiB (`XRegs` is an unboxed `[Eterm; 1 024]` = 8 192 B, `beam_process/src/registers.rs:44,53-56`) + the default collector's 233 nursery + 233 mature words ≈3.7 KiB — i.e. ≈39 KiB per live process, so the whole post-boot process table is ~1.5 MiB and **cannot** be what a boot's resident figure is made of.
- **Absent by construction**: `net_sup` and everything under it — the child spec **is** in `kernel_sup`'s list (`kernel.erl:263-268`, `start_distribution/0`) but `erl_distribution:start_link/0` is answered `ignore` (`../../native/net/connector.rs::erl_distribution_start_link`, the same answer the module's own `do_start_link([]) -> ignore` gives at `lib/kernel/src/erl_distribution.erl:131-132`), which a supervisor reads as "not started" — `dist_ac` (env-gated, `kernel.erl:261-276`), `timer_server`/`erl_compile_server` (env-gated, `kernel.erl` `start_timer`/`start_compile_server`), `pg`/`disk_log`/`boot_server`/`peer` (`kernel.app.src:163-170` sets no `start_*`), and **nothing from stdlib** (`application_controller.erl:1441-1442`: a `mod=[]` app starts no `application_master`).
Every process a directive created directly is **dead**: the three `{kernelProcess, …}` directive processes (`../drive.rs:212`, ended by `:261-263`), `heart` (`heart.erl:157-196`, `start/0` returns `ignore`), `application_master`'s one-shot "process X" (`application_master.erl:159-162` → `:231`), and `{apply, …}` runs on the runtime's own process (`../drive.rs:350`, no spawn).

## The completion-asserting tests (all pass)
The three `cargo test --lib boot::` tests (`the_real_start_boot_executes_every_directive_to_progress_started`,
`the_real_start_boot_completes_through_execute_reporting`, `the_real_boot_registers_a_live_logger_process`)
and the two `cargo test --test boot` tests (`the_real_start_boot_runs_to_completion`,
`the_cli_runs_the_real_boot_script_to_completion`) assert the FULL 22-directive completion (ending at
`{progress,started}`) and **pass**; `tests/a9_native_floor.rs::script_id_is_the_loaded_boot_scripts_real_header`
passes too. The directive-12–14 `{kernelProcess, …}` registrations and the CLI `logger` registration all
survive to completion.

## A `-boot` value's shape does not affect the outcome
The boot is **not** sensitive to the byte length or the canonical form of the `-boot` value:
`$P/bootstrap/bin/start`, `$P/bootstrap/./bin/start`, `$P/bootstrap//bin/start` and
`$P/bootstrap/../bootstrap/bin/start` all carry the real script to completion (exit 0).
**What guarantees it is the runner's parked-root publication.** A suspended process's machine lives
in `RunnerState.engines`, and a machine's registers are only a *copy* of the process's root set; the
runner therefore publishes the parked machine's whole live root set into `Process::regs` — and the
terms it holds outside that root set (the live pending-exception reason and the engine's host-held
families) as extra roots — at **every** park arm whose slice ended with a dead float-register prefix
and re-syncs the machine from
the process's authoritative root set on resume (`../../runner/run.rs::publish_parked_roots` /
`resume_from_process_roots`). Without that pair a
collection driven while the process was parked — `Process::deliver`'s exit cascade is **not** gated
by `Runtime::gc_process_enabled` — would relocate the heap against a partial root set and leave a
stale word in `x`.
**The escape-store stabilization is honest.** `../../runtime/gc.rs::try_stabilize_escaped` copies a
runtime-level store's term onto the pointer-stable arena, propagates a **genuine** copy failure as
the emulator's `error:system_limit` (never storing an un-copied term) and trips the `disable_gc`
valve **only** for the documented off-heap-payload capability gap, where the original term is kept
because it cannot be duplicated.

## Boot-path engine-entry map (which directives run code, and what a minimal boot allocates)
Verified against the code for the minimal route `beam_rt -noshell -eval 'halt().'`.
- **`execute_instruction` (`bootscript.rs:421-487`) gives the whole routing**: 3 `{kernelProcess,…}` (index 12 `heart:start/0`, 13 `logger_server:start_link/0`, 14 `application_controller:start/1`) and 4 `{apply,…}` (16 `application:load/1`, 18 and 19 `application:start_boot/2`, 20 `c:erlangrc/0`); indices 0–11, 15, 17 and 21 are load/path/progress directives that enter no module.
- **All 7 process directives reach real module code, none the host chain**: `apply_boot_call` settles routing through `resolve_external_call` (`drive.rs:363-369`) and only `Ok(None)` goes to `call_bif`; `heart`/`logger_server`/`application_controller`/`application` are none of them floor namespaces (the 14 `FLOOR_MODULES`, `native.rs`), so neither `load/1` nor `start_boot/2` is shadowed (`floor_shadows`, `native.rs:203-208`).
- **One transient `Machine` per code-resolving `{apply,…}`** — `Machine::new()` at `drive.rs:370`, dropped at the directive's end after `adopt_escaped_binaries` (`drive.rs:426`): 4 on this script.
  It is **not** recycled: `Machine::new` is a plain construction (no `Drop` returns it anywhere), and the machine pool (`context/machine_pool.rs`, `MAX_POOLED_MACHINES = 32`, `:149`) is entered only by `run_export_blocking`/`run_label_blocking`, which this path never calls — so a boot's 4 transient machines are ~26 KiB each (1 024-word `x` + 1 024-word `fr` + 256-word stack + 256×32 B frames, `beam_engine/src/machine/{registers.rs:39,mod.rs:841-845}`) allocated and freed, never retained.
- **One retained `Machine` per spawned process, created eagerly at spawn** by `RunnerState::spawn_with_stability` (`runner/entry.rs:504-535`, field `entry.rs:177`), reached from `start_kernel_process`'s `runtime.spawn_process` (`drive.rs:212`) and from every in-machine `spawn*` (the pending-spawn route); it is released by `RunnerState::prune_dead` (`runner/entry.rs:947-968`) at the first pass boundary after the process dies.
- **The live engine count is the booted tree's, not the script's**: each `{kernelProcess,…}` MFA spawns its own server in-machine (`heart:init/2`, the `proc_lib`/`gen_server` child) and directives 18/19 start the kernel supervision tree, so the number of engines at `-eval` time is an own-state answer, not a pinned constant.
- **The four `Runtime::new` processes carry no engine**: the `init` stand-in (`runtime.rs:1365`), the root process (`runtime.rs:1372`), the `erl_prim_loader` stand-in (`runtime.rs:1407`) and the native `file_server_2` (`native/file_server.rs:273`) are `ProcessTable::spawn`ed `Runnable` and never enqueued (`runtime.rs:5419`, `:5466`).
- **The `-eval` expression adds no machine, no pooled BIF heap and no nested-run machine**: `halt()` → `Evaluator::call` → `call_bif` (`eval/mod.rs:2541-2545`) → the native arm `native.rs:370` (`record_halt`), which runs before `try_call_nif` and before `call_registered`'s `take_bif_heap` (`context/calls.rs:306`).
- **The nested-run pool (`MAX_POOLED_MACHINES = 32`) starts empty and is reached only by `run_export_blocking`/`run_label_blocking`** (`context/pump.rs:294,376`), i.e. by a registered BIF that applies a fun/module export back into the engine — never by the `{apply,…}` routing above.
- **Large startup reservations (all lazily committed anonymous `mmap`)**: `bif_heaps: vec![Heap::new(heap_words)?]` (`runtime.rs:1564`) and `eval_heap` (`runtime.rs:1578`) each 128 MiB of address space (`DEFAULT_HEAP_WORDS = 1 << 24`, `config.rs:68`); `info_heap` 4 096 words (`runtime.rs:125,1576`) from a 1 MiB carrier; the `.boot` parse heap `bytes*24` words with a 4 096-word floor (`bootscript.rs:605-607`) = 169 056 words (≈1.35 MiB) for the 7 044-byte `bootstrap/bin/start.boot`, dropped when `read` returns — while the decoded script itself is only **1 474 heap words (11.8 KiB)**, so the parse heap is a ~90× over-reservation of lazily-committed `mmap`.
- **The parsed `BootScript` never outlives the boot.** `cli.rs::run_boot_script` (`cli.rs:2051-2054`) does `runtime.boot_from_script(&path).map(|_| ())`, and `boot_from_script` (`runtime.rs:3961-3978`) keeps only the `{Name,Vsn}` strings (`boot_script_id`, `:3967`) and the `PathBuf` (`boot_script_path`, `:3968`); the 22 `Instruction`s and their 202 `primLoad` name `String`s (`bootscript.rs:610-631`) drop with it. A boot's only retained share of this node is therefore those two fields + the static `&'static str` tables.
- **A directive's argument tree is built on the runtime arena and is not freed by the directive** (`boot_args`, `bootscript.rs:518-525` → `terms::make_*` on a table-less `RuntimeContext`), and the arena is rewound only by the region-reclaim pass at a `-eval` statement boundary/driver exit. Measured on the committed script: all seven process directives together account for **651 words (5 208 B)** — the kernel app spec 371 words, the stdlib app spec 280 words, the rest zero (`application:start_boot/2` takes two atoms, `heart`/`logger`/`c:erlangrc/0` take none). The `{kernelProcess,…}` ones are additionally copied into `PendingSpawn.args` (`runtime.rs:4181-4185`).
- **Small constant tables**: `PRELOADED_MODULE_NAMES` = 22 (`boot.rs:61-84`), copied into a 22-entry `Vec` by `PreloadedModules::standard` (`boot.rs:258-262`, `runtime.rs:1563`); the table's **first** seed is `erts/emulator/beam/atom.names` (`atom_seed.rs`, applied by `CodeServer::new`) — **763 distinct names** in file order — and `boot::seed_core_atoms` then appends `CORE_ATOMS` 77 + `crate::info::info_atoms()` (`INFO_ATOMS` 92 + the static-NIF roster's `SNIFS_ATOMS` 465, 4 names in both, so 553) + `PORT_INFO_ATOMS` 17 names (`boot.rs:127-237`, `info.rs`, `info/snifs_atoms.rs`, `port.rs`) plus `SignalAtoms` 9 (`boot.rs:290-312`), of which 471 are already in the file — so the whole seed is **1 234 distinct names / 12 796 bytes of name text**, i.e. low tens of KiB of `AtomTable` once each name's `Arc<str>` + index entry + chunk slot is counted. The atom population is dominated by module loads, not by this seed: every loaded `.beam` interns its whole `Atoms` chunk (`codeserver.rs:901-925`).
- **The dominant real footprint is per-module load**: 22 preloaded `.beam`s retain ≈2.1 MiB of 64-byte `Instr` arrays plus 8 B/instruction line tables, each module owns a full `AtomTable::fork()` of the shared namespace and an `mmap` literal arena sized `payload*4 + count*64 + 16` words (per-module detail in `beam_loader/CONTEXT.md`).
- **The code path is filesystem-derived, not a constant**: 36 entries under the consumed installation on this host, asserted equal to the oracle's `code:get_path/0` element-wise (`tests/code_path.rs:181-227`).

## Constraints
- A drive never spins: an exhausted budget is an honest typed error, and the outer `DRIVE_DEADLINE` is checked between slices (the four budget values are the parent's `## API Surface`).
- `drive.rs` is `#![deny(unsafe_code)]` and takes no `unwrap()` on runtime data; a failed directive surfaces through `BootStop`, never a panic.
- Every figure on this page is a static reading of the child-spec code or a measurement recorded in the ledger it names — never re-derived in a test.

## Test Strategy
- [`../bootscript.rs`](../bootscript.rs)'s `the_real_start_boot_*` lib tests and [`../../../tests/boot.rs`](../../../tests/boot.rs) drive the whole script through this module and assert completion at `{progress,started}`.
- [`../../../tests/proc_lib_start.rs`](../../../tests/proc_lib_start.rs) pins the resolve-first / blocking-aware `call_ext` seam the `{kernelProcess, …}` children depend on; [`../../../tests/logger_default_handler.rs`](../../../tests/logger_default_handler.rs) pins the booted `logger` server and its `[default]` handler set on every boot route.
- The one live port's poll budget is measured, not asserted: [`../../../PERF.md`](../../../PERF.md) *The boot path's one live-but-idle port, and the poll budget*.

## Routing Table
- `../drive.rs` → `start_kernel_process` / `apply_boot_call` / `drive_scheduler` / `install_default_logger_handler` and the four budgets.
- `../bootscript.rs` → the reader + executor that calls this module per directive ([`../CONTEXT.md`](../CONTEXT.md)).
- `../preloaded.rs` → the preloaded `.beam` loader, the other boot submodule ([`../CONTEXT.md`](../CONTEXT.md)).
- `../../runtime.rs` → `spawn_process` / `self_pid` / `as_action_process`, the state the inventory is read off.
- `../../port.rs` → the poll pass and `PortRegistry::awaits_port_reply`, the budget the hostname helper's port costs ([`../../port/CONTEXT.md`](../../port/CONTEXT.md)).

## See Also
- [`../CONTEXT.md`](../CONTEXT.md) → the boot subsystem: the directive table, the completion contract, the code-path resolution and the CLI routes.
- [`../../../tests/CONTEXT.md`](../../../tests/CONTEXT.md) → the per-suite test routing.
- [`../../../STARTUP_COST.md`](../../../STARTUP_COST.md) → what a boot's resident figure is made of.
