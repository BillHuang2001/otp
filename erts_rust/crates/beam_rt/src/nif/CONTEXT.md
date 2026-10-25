# `beam_rt` NIF host — `nif.rs`, `nif/{atoms,thread_env}.rs`, `nif_{send,select,monitor}.rs`, `build.rs`

## Intent
The runtime half of contract **A.3**: the live NIF libraries (`erlang:load_nif/2` really `dlopen`s),
the `NifHost` implementation every `enif_*` call runs on, the `NifHostServices` provider back into the
runtime, and the **process-independent** host a thread a NIF created (`enif_thread_create`) runs on.
`beam_abi` owns the ABI (`enif_*` table, `NifEnv`, the thread-env seam); this node owns *what the
runtime answers* and *where the terms live*.
The census of the ABI's per-function coverage and the residuals are `../../beam_abi/CONTEXT.md` and
`../M1_GAPS_A3_NIF.md` § *NIF (A.3)* — this file does not restate them.

## API Surface
* `nif.rs` → `NifRegistry` (the `!Send` `HashMap<String, NifEntry>` on `Runtime`; **`NifRegistry::new`
  anchors the `enif_*` codegen unit**, and `is_loaded` — not `lookup` — is the oracle's `reload`
  refusal condition), `NifEntry { lib, host }` (**field order is load-bearing**: `lib`'s `Drop` runs
  the C `unload` against the erased host pointer, so it precedes the boxed host), `NifHostImpl`
  (the `NifHost` impl over a `Heap`, the shared `Rc<AtomTable>`, a `PendingInterner`, `self_pid`,
  `schedulers`, `Option<&mut dyn NifHostServices>` and the `load_sends` window), `NifHostServices`'
  provider, and `Runtime::install_nif_thread_env`/`clear_nif_thread_env`/`drain_nif_thread_sends`.
* `nif/thread_env.rs` → `ThreadEnvFactory` (`Send + Sync + 'static`, the ABI's process-global seam) and
  `ForeignHost` (one per NIF-created thread, **not** `Send` — it owns a `Heap`); `ForeignHost::send`
  queues a complete ETF image in `ForeignSends`, which `Runtime::drain_nif_thread_sends` drains.
* `nif/atoms.rs` → `ForeignAtomHub`: the foreign threads' **private** name→index space
  (`reserve`/`lookup`/`name_of`/`scratch_table`) plus `ForeignSends`. One `std::sync::Mutex` per hub.
* `nif_send.rs` → the host half of `enif_send`: `from: Option<Eterm>` → the sender's `ProcessId` (the
  C's `from = c_p ? c_p->common.id : am_undefined`), then `NifHostServices::send` →
  `context::nif_send::deliver`. A message env is over **this** host, so `enif_make_copy` is the
  identity and the `msg_env` argument needs no handling.
* `nif_select.rs` → `enif_select`'s registration half: the resource's interest/mode/message become a
  `SelectPayload` in the runtime's one readiness store (`../port/select.rs`, `SelectWatch`).
* `nif_monitor.rs` → `enif_monitor_process`'s runtime half: the resource's `ResourceMonitors`
  (`armed: HashMap<MonitorId, ProcessId>`) and the death-time delivery.
* `build.rs` → emits `cargo::rustc-link-arg-{bins,tests}=-Wl,--export-dynamic`, which with
  `NifRegistry::new`'s anchor publishes the `enif_*` symbols a real NIF's `dlopen(RTLD_NOW)` needs
  (`../tests/nif.rs::the_binary_exports_the_enif_symbols_a_nif_imports`).

## The two atom spaces, and why the foreign one is private
- **A NIF-created thread cannot use the call path's predicted-index interner** (`PendingInterner`
  needs the live table's length at the call's start plus the "nothing else interns meanwhile"
  guarantee a call path has; a foreign thread has neither), yet `enif_make_atom` can **create** an atom.
  So its host interns into `ForeignAtomHub`, a **private** name→index space holding no reference to the
  live table. Reserving indices in the live table's own space is **unsound** — a reservation is a claim
  on an index the runtime's own appenders can reach first, and reconciling the two re-anchors an index a
  term already carries, so a live `Atom` ends up naming a *different* atom.
- **The handoff is by NAME, and it is the only crossing.** `NifHost::send` encodes the message as a
  complete ETF image against the hub's `scratch_table` (a dense `AtomTable` whose `0..n` names are the
  hub's), and the runtime decodes it against its own live table, installing the local-node identity so
  an internal pid/port/reference inside it still decodes to the internal id. An atom a foreign thread
  created is in the live table exactly when a decoded image carried it there.
- **A term built on a foreign host belongs to that host.** Its atoms carry private indices, so
  `beam_term`'s index equality would compare them against live atoms wrongly; it may be read back there
  and sent from there, and must never reach the runtime's own term machinery. The one shape that could
  smuggle one in — `enif_send` from a **call-path** env carrying a **foreign**-host term — is refused
  by construction (the call host encodes against the live table, where a private index names nothing),
  a typed refusal rather than a substituted atom.
- **No runtime root enumeration touches the foreign heaps.** They are never rewound (a NIF may still
  hold a term on one and the host cannot know when it stopped), which is also why every term leaving
  them is copied out by encode/decode.

## Constraints — the raw-word inventory (every site, with its region and its lifetime)
The crate-wide rule is `../../CONTEXT.md` § *Host-side term stores*; this is the `nif*` half, verified
by reading the fields.
- **Per-call `NifHostImpl` heap = the pooled BIF heap** (`context/calls.rs` `take_bif_frontier` →
  `put_bif_heap`); the load/unload host's heap is a dedicated `beam_alloc::Heap` owned by
  `NifEntry.host`. Neither region is compacted, so both hosts' words are stable by construction — the
  only terms on this subsystem that sit in a *rewritable* store are the `stabilize_term` copies below.
- **`nif.rs` `NifHostImpl.self_pid: Eterm`** — a pid **immediate**, no heap word; lives as long as the
  host (library lifetime for the load host).
- **`nif.rs` `NifHostImpl.load_sends: Option<Vec<PendingNifSend>>`**, where
  `PendingNifSend { from: ProcessId, to: Eterm, msg: Eterm }` — `msg` names the **load host's own
  heap**. The window is per-load-callback: `enif_send` pushes, `take_load_sends` drains
  (`NifBridge::load_nif`, `context/bridges.rs`, drains it the moment the callback returns, over the
  pass's own process table), so the `Vec` is never runtime-lifetime.
- **`nif.rs` `NifHostImpl.envs: Vec<Box<NifEnv>>`** — each env's `pending_exception:
  Cell<Option<Eterm>>` (`beam_abi/src/nif/host.rs`) and `recorded_atoms:
  Rc<RefCell<Vec<(usize, String)>>>` (raw atom **word**; atoms are immediates) are stores made through a
  **shared** env reference (a raise, an atom record) — the aliasing contract `../../beam_abi/CONTEXT.md`
  § *The aliasing contract on both C ABI envs* pins them. An env a NIF leaked lives as long as the host.
- **`nif.rs` mints reference TERMS into `Runtime::references: HashMap<Eterm, Option<MonitorRef>>`**
  (`make_ref`, `monitor_process_result`). `Eterm`'s `Hash` is the raw word, so those keys are the
  reference block's **address** on the host heap: address-identity keys, which a compaction of those
  regions would break (`../CONTEXT.md` `references` row).
- **`nif_select.rs` is the subsystem's ONE `stabilize_term` copy**, and `Runtime::stabilize_term`
  copies onto **`Runtime::eval_heap`** (not the arena, whatever an inline comment on that call site
  says): the select registration's `reference` (or its `CUSTOM_MSG` message) becomes
  `SelectPayload::{Plain,Custom}` in `SelectWatch.registrations` (`../port/select.rs`). **The one
  cross-file landing point of the `-eval` store outside `eval/`**, so a store move must relocate it —
  it is enumerated as `RootFamily::SelectPayload` in `runtime/roots.rs`. The sibling
  `SelectRegistration.handle: Eterm` is `make_small(fd)` (an immediate) and `obj` is a `*mut c_void`.
- **`nif_monitor.rs` `ResourceMonitors.armed: HashMap<MonitorId, ProcessId>`** — `MonitorId` is a
  `[u8; REF_THING_BYTES]` **byte copy** of the ref block (value identity) plus a `ProcessId`: no term,
  no address, so no relocation obligation.
- **The foreign hosts' heaps are the subsystem's only non-enumerated term homes**, and nothing on them
  is visible to the runtime (see the atom-space section). `ForeignAtomHub` stores names and indices,
  never an `Eterm`.

## Design Decisions
- **`enif_send` from a NIF-created thread is a queued ETF image, not a synchronous delivery.** The C
  delivers inside the call (`env == NULL` ⇒ sender `am_undefined`, queued before `enif_send` returns);
  here the hook answers the C's `1` at `enif_send` time and the message reaches the mailbox (and wakes
  a parked receiver) at the runtime's **next pump**. The observable ordering "in the mailbox by the time
  `enif_send` returned" is therefore not reproduced — the only shape available, since the receiver's
  heap/mailbox/process table belong to the runtime thread. The C's zero-copy heap splice
  (`erts_create_message_from_nif_env`) is likewise unavailable: a foreign term lives on a heap the
  runtime may not read and carries a private atom index, so an ETF image is the one representation that
  crosses the boundary by name.
- **The factory slot is process-global and last-install-wins.** `install_nif_thread_env` replaces (and
  drops) any previous factory and hands this runtime a **fresh** queue; `clear_nif_thread_env` drops it.
  `install` answers `false` only for a **poisoned** ABI factory lock, storing nothing — a poisoned
  install can never be turned into a success. The crate's suites serialise installs with
  `crate::runtime::lock_nif_thread_env`.
- **The ABI caches the opened host on its thread under the factory's generation stamp**, so an install
  or clear bumps the generation and a host opened by a superseded factory is dropped on that thread's
  next use. A NIF must not keep an env (or a term built on it) across a factory reinstall.
- **`ForeignHost::alloc_env` builds the env over this host**, so a term from *another* thread's
  `ForeignHost` heap is not valid here; `NifHost::send` (which encodes) is the only cross-thread route.

## Known Issues
- **`NifHostImpl`'s callback-visible state is plain, not interior-mutable**, sound **only** because this
  runtime never keeps a `&`/`&mut` to a host alive across a call into NIF code (the per-call host is an
  owned local in `context/calls.rs`, the load/unload host is owned by `NifEntry`, and each `NifEnv` is
  reached only through the env's own pointer). The precise failure precondition and the env-side release
  gate are `../CONTEXT.md` § *Known Issues* and `../../beam_abi/CONTEXT.md`.
- **The load/unload host runs with `services = None`** (only the callback-scoped `enif_send` window
  answers there), `upgrade` is not modelled (a second `load_nif/2` is the oracle's `reload` refusal), and
  a `dtor`-time send has no drain point.
- **`nif_yield_point` is a no-op on both hosts**, so a self-rescheduling `enif_schedule_nif` chain keeps
  its thread until it ends — a fairness bound, never a wrong result.
- **The remaining per-function divergences** (the `enif_thread_type()` driver-thread answer, a remote
  `enif_monitor_process` target answering `NotAlive`, a copied `enif_make_resource_binary` term not
  resolving through `enif_get_resource`, and the observation tally that accounts for them) are
  `../M1_GAPS_A3_NIF.md` § *NIF (A.3)* and `../../beam_abi/CONTEXT.md`.

## Test Strategy
Crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt`.
The NIF suites are oracle-cross-checked against `cc`-built C fixtures (each **skips loudly** without the
toolchain): `../tests/nif_a3_{thread_env,schedule,resource_stop}.rs` (the thread-env behaviour above, the
schedule chain, the STOP verdict), `../tests/nif{,_send,_send_load,_select,_monitor}.rs` (the exported
symbols + `load_nif/2`, the `enif_send` routes, the readiness store, the armed monitors),
`../tests/nif_resource_{gc,nested}.rs` and `../tests/nif_{term_image,binary_to_term_used}.rs`, plus the
colocated `nif/atoms_tests.rs` and `nif/thread_env_tests.rs` for the hub's index space and the
factory/generation rules. Aliasing (`host_aliasing_tests`) must be run in **both** profiles — the dev
profile is `opt-level = 1`, not a proxy for release.
Per-suite routing: `../tests/CONTEXT.md`; the test recipes: `../TEST_STRATEGY.md`.

## Routing Table
* `nif.rs` → `NifRegistry`, `NifEntry`, `NifHostImpl`, `NifHostServices`' provider, the three
  thread-env runtime entry points (`install_nif_thread_env`/`clear_nif_thread_env`/
  `drain_nif_thread_sends`), and `NifBridge`'s dispatch (`context/bridges.rs`).
* `nif/thread_env.rs` → `ThreadEnvFactory`/`ForeignHost`, the ETF-image encode of `enif_send`,
  `ForeignSends`.
* `nif/atoms.rs` → `ForeignAtomHub` (the private name→index space + its cached dense `scratch_table`).
* `nif/atoms_tests.rs` / `nif/thread_env_tests.rs` → the colocated suites.
* `nif_send.rs` → the `enif_send` host half (sender resolution + `context::nif_send::deliver`).
* `nif_select.rs` → the `enif_select` registration half (`SelectPayload` into `port/select.rs`).
* `nif_monitor.rs` → the `enif_monitor_process` armed set and its death-time delivery.
* `build.rs` → the `--export-dynamic` link arg that publishes the `enif_*` symbols.
* Read-only cross-repo: `../../beam_abi/src/nif/{host.rs,enif/,thread_env.rs}` (the ABI half),
  `../../../../erts/emulator/beam/erl_nif.c` (ground truth: `:1006`, `:876-877` for `enif_send`'s
  sender/mailbox ordering).

## See Also
- `../CONTEXT.md` → the crate-level A.3 status, the NIF caveats and the dispatch order.
- `../../beam_abi/CONTEXT.md` → the ABI half: the `enif_*` table, the aliasing contract, the residuals.
- `../M1_GAPS_A3_NIF.md` → the A.3 gap ledger and the observation tally.
- `../context/CONTEXT.md` → `try_call_nif`, the load-callback drain site and the NIF-resource prune.
