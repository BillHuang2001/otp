# `beam_rt/src/native/erts_internal/` — the `erts_internal:*` floor

## Intent
The `erts_internal` namespace (contract A.7) of the runtime-native floor: the primitives the preloaded Erlang modules call rather than the user-facing BIFs.
`erts_internal.rs` (the sibling parent file) holds `RuntimeContext::erts_internal_call(function, arity, args) -> Option<Result<Eterm>>` and the served set's bodies; `trace.rs` (this node) holds the `trace/3` + `trace_pattern/3` handler bodies and their private pattern/target helpers, over `crate::trace`.
`beam_bifs` owns exactly **one** `erts_internal` BIF (`cmp_term/2`), left to the registry on purpose.
The dispatch is `../native.rs`'s: one `try_native` arm for the whole namespace, the one dispatch table — this node adds no second list.

## API Surface
### The served set — `../erts_internal.rs`
The served keys (per-BIF oracle probes in `erts_internal.rs`'s rustdoc): `term_type/1`, `get_dflags/0`, `beamfile_chunk/2`, `beamfile_module_md5/1`, `map_to_tuple_keys/1`, `binary_to_integer/2`, `list_to_integer/2`, `time_unit/0`, `perf_counter_unit/0`, `get_creation/0`, `is_process_alive/1,2`, `process_flag/3`, `group_leader/2,3`, `no_aux_work_threads/0`, `processes_next/1`, `term_to_string/1,2`, `trace/3`, `trace_pattern/3`, `garbage_collect/1`.
* `get_dflags/0` ⇒ the fixed record `{erts_dflags,468283523004,17230663572,468283523004,8396866, 8192}` on a non-distributed and a `-sname` node alike (`beam_dist::flags`'s constants narrowed to `i64` at the emit site), its `term_to_binary/1` image byte-exact to the oracle's.
* `beamfile_chunk/2` (`beam_bif_load.c:165-201`) ⇒ a named chunk's payload as a **binary**, `undefined` for an absent/zero-length chunk or an unparseable container (a duplicate tag resolving to the **last** occurrence), `badarg` for a non-binary first arg / a name that is not a list of small ints; chunk ids are the 4-byte ASCII tags (**case-sensitive**), read through the public `beam_loader::container`.
* `beamfile_module_md5/1` (`beam_bif_load.c:203-229`) ⇒ the module checksum as a **bare 16-byte binary**, **not** `beam_lib:md5/1`'s `{ok,{Module,Md5}}` wrapper; `undefined` for an image `beamfile_read` cannot parse (a malformed container, or a `FunT` chunk that is not `4 + 24n` bytes), `badarg` for a non-binary argument; the digest is `crate::context::module_md5` read from the argument binary, never a stored value (`../../context/bridges.rs`, re-exported `pub(crate)` by `../../context/mod.rs`).
* `map_to_tuple_keys/1` ⇒ a **flat** map's keys tuple (atom keys in atom-index order), `badarg` for a hashmap, `error:{badmap,Term}` (structured live reason) for a non-map.
* `binary_to_integer/2` ⇒ a small int, `big` or `badarg`; `list_to_integer/2` ⇒ `{Value, Rest}`, else `no_integer`/`not_a_list`/`badarg`/`big`; `time_unit/0` and `perf_counter_unit/0` are both `1000000000`; `get_creation/0` ⇒ `undefined` on a non-distributed node, else the creation integer.
* `is_process_alive/1` ⇒ `true`/`false` for an internal pid, `badarg` otherwise; `/2`, the reference-carrying variant Erlang's `/1` wrapper builds on, ⇒ `ok` for a **live and a dead** pid (an argument test plus a signal send, never a table lookup), `badarg` for a non-pid / a non-runtime-reference.
* `process_flag/3` (`bif.c:2262-2297`, `process_flag_aux:1896-2022`) is the bare `process_flag_aux` surface for the **calling** process, implementing exactly the `save_calls` flag: `(self(), save_calls, N)` ⇒ the previous length (`0`, `7`, `4`, …); a non-small/out-of-range value or the `trap_exit` flag ⇒ `error:badarg`; a non-pid target ⇒ the value `badtype`; another process ⇒ the typed async-RPC gap (deliberately **not** `erlang:process_flag/2`).
* `no_aux_work_threads/0` = this runtime's scheduler count `+ 1` (no dirty-allocator instances modelled), **diverging** by being *this* runtime's own count (`crate::info::schedulers_count`) — oracle-exact for every explicit `+S N`.
  `processes_next/1` ⇒ `{NextIndex, [Pid]}` / `{Limit, []}`, `badarg` past the limit, **diverging** by being index-addressed over *this* table, one pid per call (the C ptab is paginated).
* `term_to_string/1` is the `~w` rendering as a string, one line, no newline; `/2`'s `undefined` limit form is exact, a **numeric** limit is a typed `unsupported` (the `~P` depth directive is not exposed by `../../format.rs`), a limit `<= 1` or a non-integer/non-`undefined` limit is `badarg`.
* `trace/3` is the body of `erlang:trace/2,3`: `PidSpec` selects the tracees (a pid the table holds, or the group atoms `all`/`existing`/`existing_processes`/`processes`), `How` sets/clears their flags, `FlagList` is the flag set, the **caller** becomes the tracer, and the tracee count is returned; `trace_pattern/3` installs/removes a per-function pattern over `MFA` (each part a name or the `'_'` wildcard) via `crate::trace`'s `PatternTable`; the port/`new`-style `PidSpec`s and a non-`true`/`false` `How` are the typed/`badarg` paths.
* `group_leader/2,3` route through `Runtime::group_leaders` (the arm's own half is `../group_leader.rs`); `garbage_collect/1` records the collection request rather than collecting ([`../process/CONTEXT.md`](../process/CONTEXT.md) *`erlang:garbage_collect/0,1,2`*).
* **Divergence:** `term_type/1` reports every byte-aligned binary as `sub_binary` — the real `heap_binary`/`sub_binary`/`refc_binary` split is the boxed header subtag (`erl_map.c:3368-3376`), readable only through the `unsafe` `beam_term::boxed` this crate's `deny(unsafe_code)` forbids; a typed `unsupported` stays for the boxed families the safe API cannot name (an internal `reference`, a non-byte-aligned bitstring).

## Not served — the `erts_internal` families left falling through
Every name not in the served set answers `None` and falls through to the `beam_bifs` registry then a real module load — an honest failure, never a fabricated value.
The floor's one index of these keys is `../CONTEXT.md` *Not implemented*; this section carries the reasons.
- **Distribution primitives (A.5 phase 2)** — the connection/channel-negotiation, dist-spawn and dynamic-node-name family: needs the controller protocol, a live node identity and remote spawn. The **`net_kernel` name itself is held** by this crate's native service (`../net_kernel.rs`), so a peer's `gen:call` `{is_auth, Node}` is answered there.
- **Tracing, debugger half only** (`trace_session_create/3`, `trace_session_destroy/1`, `trace/4`, `trace_pattern/4`, `trace_info/3`, `notify_breakpoint_hit/3`, the `erts_debug`/`erl_debugger` breakpoint entries, `erts_trace_cleaner:*`): the **process-tracing core IS served**; the `/4` + `trace_info/3` surfaces would add a flag/match-spec dimension `crate::trace` does not expose.
- **Microstate accounting / scheduler wall time** — needs per-scheduler run-time accounting (the runtime is single-threaded); `erts_internal:scheduler_wall_time/1` itself **is served** by the `RuntimeHooks` bridge.
- **System tasks / system monitors** — needs the system-task protocol; `is_system_process/1` **is served** by the same bridge (`is_process_predicate`), whose divergence is that the oracle's `true` for `init` is unreachable because nothing calls `beam_process::ProcessFlags::set_system`.
- **Code purge / literal areas** — `erts_internal:purge_module/2` stays the oracle's `error:notsup` for a direct caller (`../../../../../../erts/emulator/beam/beam_bif_load.c:2088-2092`) and `erts_literal_area_collector:release_area_switch/0` is not served at all: the literal-area release handshake needs a stack scan for module references and the `erts_code_purger` process this runtime does not register (`erlang:whereis(erts_code_purger)` ⇒ `undefined` where the oracle answers `<0.1.0>`).
  The two **queries over the old-code slot** are served by the floor instead: `code:purge/1` ⇒ `false` and `code:soft_purge/1` ⇒ `true` for an atom (loaded, unloaded or just deleted — measured on OTP 29.1.1 / `erts-17.1`), the bare-atom `error:function_clause` for a non-atom (`code.erl:722`/`:740`'s `is_atom/1` guard), so the live `code_server` survives a purge call instead of raising `notsup` inside itself (see `../../../M1_GAPS.md` *The code store*).
- **ETS internals** (`ets_lookup_binary_info/2`, `ets_raw_first/1`, `ets_raw_next/2`) — need the raw `term_to_binary` **block** writer / the `db_raw_first|next` key iterator, which `beam_ets`' public API does not expose; `ets_super_user/1` needs an ETS-protection-bypass bit on `beam_process::ProcessFlags`; `ets_info_binary/1` is **not a BIF** (absent from `bif.tab`).
- **Allocation/IO introspection and misc** — `gather_alloc_histograms/1`, `gather_carrier_info/1`, `gather_io_bytes/2` and `crasher/6` need the `erts_alloc` carrier/histogram readers, the async-thread pool and the crash-dump path; `get_internal_state_blocked/1` is **pure Erlang**.
- **Process/scheduler control** — `process_display/2`, `suspend_process/1,2`, `await_result/1`, `flush_monitor_messages/3`, `'@flush_monitor_messages_refopt'/0` are [`../process/CONTEXT.md`](../process/CONTEXT.md)'s *Not served* section.

### Served elsewhere — the floor must NOT claim them
`atomics_new/2`, `counters_new/1`, `counters_add/3`, `counters_get/2`, `counters_info/1`, `counters_put/3` are the `beam_bifs` registry's.
`erase_persistent_terms/0` is the `RuntimeHooks` bridge's.
`map_hashmap_children/1`/`map_next/3` are `beam_bifs/src/erts_internal.rs`'s.
Claiming any of them here would shadow a registry BIF (the floor's standing constraint).

## Constraints
- The crate-wide `#![deny(unsafe_code)]` covers this node; terms are built and read only through `beam_engine::terms`, `beam_codec::term`, `beam_loader::container` and `beam_term`.
- Return-based errors: `Option<Result<Eterm>>`, never a crate-local error enum, never `panic!`/`unwrap()` on runtime data, never a silent swallow; a value atom the C returns (`badarg`, `big`, `no_integer`, `not_a_list`, `badtype`, `no_integer`) is `Ok(atom)`.
- The dispatch stays in `../native.rs` — `erts_internal_call`'s arms are registered there, and the served set above is the namespace's contract, never a second dispatch list.
- A BIF here must never shadow a `beam_bifs` registry BIF (the `Served elsewhere` list is the one exception set, and it is served elsewhere *because* the registry owns it).

## Test Strategy
- The colocated unit suite lives with `../erts_internal.rs` and drives the floor through the crate's own dispatch (`../test_support.rs::call` ⇒ `call_bif` ⇒ `try_native`), so the served surface is pinned rather than an internal function.
- `trace.rs` has no colocated tests: the tracing behaviour is pinned by `../../../tests/trace*.rs` (emitted vs deferred events, per-module routing — [`../../TRACE_AND_PERF.md`](../../TRACE_AND_PERF.md)).
- `get_dflags/0` is pinned by byte-comparing the built term's `term_to_binary/1` image against the oracle's; `beamfile_chunk/2`/`beamfile_module_md5/1` are pinned over real `.beam` images.
- Oracle values are quoted in each test's doc comment (re-verified against the system `erl`, OTP 29.1.1 / erts-17.1); a typed-gap row pins the `unsupported` outcome with a `// documented divergence:` comment.

## Routing Table
- `../erts_internal.rs` → `erts_internal_call` and the served set's bodies (incl. the `trace/3` + `trace_pattern/3` dispatch arms) + its unit tests.
- `trace.rs` → the `erts_internal:trace/3` + `:trace_pattern/3` handler bodies and their private pattern/target helpers, over `crate::trace`.
- `../native.rs` → the single `erts_internal` namespace arm of `try_native`.
- `../../trace.rs` → `PatternTable`, the tracee flags and the trace event routing.
- `../../context/bridges.rs` → `module_md5`; `../../format.rs` → the `~w` renderer.
- `../../../../../../erts/emulator/beam/` → the C ground truth: `beam_bif_load.c` (`beamfile_chunk/2`, `beamfile_module_md5/1`, `purge_module/2`), `bif.c` (`process_flag_aux`), `erl_map.c` (`term_type/1`'s family table), `dist.c` (`get_dflags/0`, `get_creation/0`).

## See Also
- `../CONTEXT.md` → the floor's contract, the dispatch order, the complete not-implemented index and the constraints.
- `../process/CONTEXT.md` → the process-control families (`garbage_collect`, `group_leader`, the process/scheduler-control gaps).
- `../../../../../../erts/emulator/beam/` → the reference implementation (read-only).
