# `beam_io` integration tests (`erts_rust/crates/beam_io/tests/`)

## Intent
Every integration suite of the `beam_io` crate (the I/O subsystem / OS-driver seam). Each
`tests/*.rs` file is its own Cargo test binary and asserts the crate's public seams against **real**
OS facilities (pipes, sockets, files, signals, `dlopen`ed C objects), never against a mock.
The crate-level routing table (parent node `../CONTEXT.md`) carries the authoritative list of
suites and helper entry points; this node adds the **fixture-level** knowledge a test author needs
(the C fixtures, their control-verb vocabulary and their variant-macro pattern).

## API Surface (node-local)
- `support/mod.rs` — toolchain discovery and the loud-skip policy: `erl_include_dir()`,
  `cc_available()`, `compile_so()`, `scratch_dir()`, `skip()`, `probe_driver_name()` (a
  `libc`-only `dlopen` + `driver_init` read, used where a `beam_abi` dependency is not available
  to the test).
- `cdriver/mod.rs` — the shared C-driver helpers: `load_driver`/`load_named_driver`/`open_port`/
  `open_named_port`/`round_data`/`data_of`/`reads_of`/`closed_of`/`await_data`/`drive_until`/
  `await_log_line`/`logged_port_command`/`LoggedPort` and the fixture builders
  (`fixture_so`, `variant_fixture_*`, `no_process_exit_variant_fixture_*`, `vector_fixture_so`,
  `both_fixture_so`, `shape_fixture_*`).
- `fixtures/*.c` — the C driver objects. `host_driver.c` is the base; the others `#include` it
  after defining a `HOST_DRIVER_*_SLOT` override macro, so one file carries the whole vocabulary.

## The C fixtures (`fixtures/`)
- **`host_driver.c`** (`driver_name = "host_driver"`) — the base fixture. `start` parses
  `fd=<n>`, `log=<path>`, `upper` and `hdl`, plus the bare mode tokens `silent` (consume the command
  and emit nothing) and `busy` (call `set_busy_port(port, 1)` at start time). Its integer `control`
  verbs (documented at the top of the file, the authoritative list) cover `driver_output`, timers,
  `driver_select` + `ERL_DRV_USE` release, `erl_drv_output_term`, `driver_async`, the driver-queue
  family, a `ERL_DRV_WRITE` arm and the busy state:
  `19` `driver_enq`, `20` `driver_sizeq`, `21` `driver_peekq`, `22` `driver_deq`,
  `23` `driver_peekqv`, `24` `driver_select(WRITE)`, `25` `set_busy_port(port,1)`,
  `26` `set_busy_port(port,0)`, `27` the `control` reply that sets `*rbuf = NULL` and returns
  `cres > 0`. Verbs 19–23 answer `key=value` text with a trailing raw `bytes=` payload (the queue
  family's observation channel); 25/26 answer an empty reply, 27 must render as `[]`.
- **Variant macro pattern.** Each overridable `ErlDrvEntry` slot has a `HOST_DRIVER_<SLOT>_SLOT`
  macro; a variant defines it *before* `#include "host_driver.c"`, which both selects the callback
  and omits the unused function. `HOST_DRIVER_OUTPUTV_SLOT` follows the same pattern with its
  default flipped to `NULL` (the base entry is an `output`-only driver).
- **`host_driver_no_ready_async.c`** — `ready_async = NULL` (the `async_free` arm).
- **`host_driver_no_process_exit.c`** — `process_exit = NULL` (the `emergency_close` fallback).
- **`vector_driver.c`** — the `outputv`-only entry (`outputv` set, `ready_output = NULL`).
- **`host_driver_both.c`** — `host_driver.c` with `HOST_DRIVER_OUTPUTV_SLOT = host_outputv`, the one
  build whose entry declares `outputv` **and** `output` **and** `ready_output` at once: the shape
  the C's `outputv`-over-`output` preference (`../../../../erts/emulator/beam/io.c:1782`) is about.
  `host_outputv` logs `outputv`, `host_output` logs `output`, `host_ready_output` emits `"W"` — so
  which callback ran, and in which order, is observable from the log + the port data.
- **`output_shape_driver.c`** (`driver_name = "output_shape_driver"`) — the **scripted-opcode**
  fixture: its `output` callback reads the *first command byte* as an opcode and makes the
  `driver_output*`/`driver_failure*` call that opcode names (the whole 0…15 table is in the file
  header), so one `port_command` drives exactly one scripted C call site and the resulting
  `{Port,{data,_}}` / port-exit pair is the observable. It logs `output op=<n>` plus one
  `call=<name> rc=<n>` line per C call, so a *refused* call (`rc=-1`) is distinguishable from a
  delivered one. Rows 0–4 are the read **shapes** (`""`/`NULL`/header-only/header+empty
  body/header+body), 5 is two calls in one callback, 6–9 the four `driver_failure*` reasons,
  10–11 the two `driver_exit` codes, 12–14 the ordering/refusal rows, 15 a single six-byte read
  (the `{line,4}` over-limit row). Its escript probe is generated per run by
  `write_oracle_escript()` and needs `escript` as well as `erl`.

## Constraints
- **Skip loudly, never fail on a missing toolchain.** A suite needing `cc`/`erl_driver.h`/`erl`
  prints a `SKIP:` banner and returns; a *load* failure is never a skip (it means the `driver_*`
  symbols are missing from the test binary's dynamic symbol table). The `shape_fixture_so()` builder
  follows the same rule (`SKIP:` without `cc`/`erl_driver.h`), and the one escript-based oracle case
  additionally needs `escript` on `PATH`.
- **A C-driver test binary must force the `driver_*` symbols into its own `.dynsym`**
  (`force_driver_symbols_link()` / a `driver_api_table()` reference) *and* rely on `build.rs`'s
  `-Wl,--export-dynamic`; either one alone makes `dlopen(RTLD_NOW)` fail with
  `undefined symbol: driver_mk_port`.
- The selected descriptor handed to `fd=<n>` must be **non-blocking** (the reactor is
  edge-triggered and the fixture's `ready_input` reads down to `EAGAIN`).
- Signal tests are serialized on a global `Mutex` (the facility is process-global) and never raise
  a fatal signal to self.
- Every `*.rs` file carries the OTP Apache-2.0 header block; the `*.c` fixtures deliberately do
  **not** (only `*.rs` is in the license scanner's scope).
- Tests only exercise the public `beam_io` API — no `src/` edits.

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` (never a
bare `cargo`, never `cd` — use the wrapper, the sandbox `$HOME` is read-only). 24 suites, **219
passing / 0 failing**; the whole `-p beam_io` run is **958** (736 lib + 219 integration + 3
doctests, 2 doctests `ignore`d). Per file:

| Suite | Tests | Suite | Tests |
|---|---|---|---|
| `abi_driver.rs` | 21 | `spawn.rs` | 30 |
| `abi_async.rs` | 8 | `signal.rs` | 21 |
| `abi_vector.rs` | 7 | `efile.rs` | 16 |
| `driver_output2.rs` | 6 | `ram_file.rs` | 12 |
| `driver_table.rs` | 5 | `ddll.rs` | 11 |
| `driver_seams.rs` | 7 | `poller.rs` | 11 |
| `abi_lifecycle.rs` | 3 | `cpu_time.rs` | 8 |
| `driver_registry_slots.rs` | 3 | `fixture_builds.rs` | 8 |
| `abi_busy.rs` | 2 | `inet.rs` | 8 |
| `abi_start_failure.rs` | 2 | `inet_async.rs` | 8 |
| `abi_failure.rs` | 8 | `udp_inet.rs` | 5 |
| `port_task.rs` | 5 | `port.rs` | 4 |

Two suites pin the **`{inet_async, …}` / `{inet_reply, …}` completion channel** end-to-end
(`PortRegistry::take_completions` after every round, the recipe `beam_rt` follows):

- **`inet_async.rs` (8)** — the `tcp_inet` asynchronous-op queue over real loopback sockets. Rows
  cross-checked against the `erl` oracle: `the_receive_guard_ladder_agrees_with_the_erl_oracle`
  (six `TCP_REQ_RECV` replies byte-compared, the oracle's asynchronous connect offsetting the ids),
  `the_caller_tag_round_trip_agrees_with_the_erl_oracle` (the oracle's own
  `term_to_binary(make_ref(), [local])` tag, both peer reads, the 4-tuple `{inet_reply,_,ok,Ref}`
  and the silent zero-length tag) and `closing_a_port_with_an_operation_pending_completes_it_once`
  (the oracle's reason; its message *count* is a probe property and stays unasserted). Rows that are
  structural-only (no oracle column): `a_receive_hands_out_an_id_and_the_completion_never_rides_the_reply`,
  `a_receive_completes_at_the_rounds_async_drive_point`,
  `an_accept_completion_hands_over_a_descriptor_the_runtime_can_adopt` (the descriptor hand-off;
  `INET_REQ_FDOPEN` adoption is a crate-local route to the same state the C reaches internally) and
  `port_notifications_stay_exactly_two_variants` (the compile-time two-arm match).
- **`udp_inet.rs` (5)** — `the_code_60_recv_is_an_async_request_completed_by_a_later_round` is the
  datagram half, byte-compared against a real raw `udp_inet` port on every run-specific-independent
  row: the `[1,0,0]` id reply with nothing staged, `[0]ealready` for a second concurrent receive
  (which still consumes an id, so the next request is `[1,0,2]`), `[0]exbadport` socket-less,
  `[0]einval` for a `len != 8` operand, one `InetAsyncResult::Data` completion at the
  `execute_port_tasks` drive point with `read.flattened()` = `[1,PortHi,PortLo,A,B,C,D,…data]` (the
  address prefix is shape-compared, each side checking its own sender's port), and `{error, closed}`
  exactly once for a receive pending at close. The file's other oracle case
  (`the_covered_control_replies_agree_with_the_erl_oracle`) is the open/bind/setopts/getopts/close
  gate.

**`abi_failure.rs` (8)** is the one suite that pins the **`driver_output*` shapes and the
driver-failure channel** over the scripted-opcode fixture (`output_shape_driver.c`) rather than
`host_driver.c`: one port per row (the failure rows close their own port, so a shared port would
answer `badarg` on the next command), one `port_command` = one scripted C call site, and the round
is inspected through `reads_of`/`closed_of` plus `PortRegistry::take_failures`. It asserts the
split (`body: None` ≠ `Some(vec![])`, two calls in one callback = two messages), the `{line,4}`
column, every `FailureReason` and its `{'EXIT',·,Reason}`-equivalent, `driver_exit`'s
`signals_owner == false`, the data-before-failure ordering, a delivery after a failure being
refused with the bytes reaching nobody, and the reaped port's number being released.
`the_rendered_rows_match_the_erl_oracle` re-derives the whole 16-row table live from the system
runtime and compares it with the crate's own rendering, so the table in the test's header cannot
rot.

## Known Issues / Notes for Agents
- `driver_peekq`/`driver_peekqv` report `vlen == 1` on the Rust host (`AbiHost` publishes the whole
  contiguous queue as one iovec entry) where the oracle counts one entry per `driver_enq`.
- `AbiHost::pushq` flattens the queue (`front()` + clear + push), so byte order after a
  `driver_pushq` is not the C's; no fixture verb drives `driver_pushq` and no test asserts its
  order.
- The fixture's per-callback `log=<path>` is append-mode and per-run scratch directories keep runs
  isolated; `stop_select` cannot see `ErlDrvData`, so it uses a file-scope log path — one port per
  test.
- **An `erl` oracle probe must talk to the driver port it opened.** A `tcp_inet` probe that queues
  `INET_REQ_ACCEPT` has to connect its peer to the port's *own* `bind` reply, not to a `gen_tcp`
  listener — otherwise the accept never completes and the probe exits. Read exactly the one message
  each step owes (the accept's `{inet_async, _, _, {ok, Port2}}` first, then the command's
  `{inet_reply, _, ok, Ref}`), and on `port_close` skip the ownership `{'EXIT', Port, normal}` that
  may precede the pending operation's `{inet_async, _, _, {error, closed}}`.
## Routing Table
- `abi_driver.rs` → the end-to-end C-driver path (incl. the driver-queue family round-trip, the
  `outputv`-over-`output` preference, the empty-queue write-readiness case, the `NULL`-`*rbuf`
  `control` reply and the silent-driver output pin)
- `driver_output2.rs` → the `driver_output2`/`driver_output_binary` **header/body split** through a
  real `PortRegistry`: one driver call = one `ReadMessage { header, body }` notification, the empty
  header vs `body: None` distinction, two calls in one callback = two messages in arrival order,
  `input_len` counting the header, the `{line, N}` frame header stamp, and the flat
  `DriverContext::input()` view- `abi_busy.rs` → the `set_busy_port` state half (the raw API-table hook + the fixture's `busy`
  token and verbs 25/26)
- `abi_async.rs`, `abi_vector.rs`, `abi_lifecycle.rs`, `abi_start_failure.rs` → the `driver_async`
  completion arms, the `outputv`-only entry, the port-exit adapter, a failing `start`
- `abi_failure.rs` → the `driver_output*` shapes + the driver-failure channel over
  `fixtures/output_shape_driver.c` (one `ReadMessage` per call, the `body: None`/`Some([])`
  distinction, every `FailureReason`, `driver_exit`'s no-signal shape, the
  data-before-close ordering, the post-failure refusal, the id release, and the live 16-row
  oracle table)
- `driver_table.rs`, `driver_registry_slots.rs` → the process-global driver registry
- `poller.rs`, `port.rs`, `port_task.rs`, `driver_seams.rs` → the reactor, the port, port-task
  scheduling, the driver-declared timer/port-exit seams
- `inet_async.rs` → the `tcp_inet` **asynchronous-op queue + completion channel** through
  `PortRegistry` (numeric opcodes, the single receive slot, the caller tag, the accept hand-off and
  its `INET_REQ_FDOPEN` adoption, the `{error, closed}` teardown); three `erl`-oracle cases
- `efile.rs`, `spawn.rs`, `inet.rs` → the built-in drivers on real files, real children, loopback TCP
  (`spawn.rs` also cross-checks the port-level `[line]`/`[binary,{line,N}]` framing against the oracle)
- `udp_inet.rs`, `ram_file.rs` → the other two built-ins through the real port layer
  (`DriverRegistry::with_builtins` + `PortRegistry` + `Poller`): a loopback UDP datagram round trip
  (`udp_inet`) and the packed opcode/`control` command grammar (`ram_file_drv`), each with an
  `erl`-oracle cross-check over the reply bytes that skips loudly without `erl`. `udp_inet` also
  pins the **`PACKET_REQ_RECV`(60) asynchronous** receive (id reply, `ealready`/`exbadport`/`einval`
  ladder, the `Data` completion with the packed source address as the read's header, the
  `{error, closed}` teardown) against the real datagram driver
- `signal.rs`, `cpu_time.rs`, `ddll.rs` → the OS-signal facility, the CPU-time facility, `dlopen`
- `fixture_builds.rs` → builds + loads the `host_driver`-family fixtures (the base, the two
  `ready_async`/`process_exit` variants, the `vector` and `both` write-side shapes) plus the
  negative cases (a `cc` failure is reported, a missing object is an error, the OTP header is
  locatable); a broken fixture is a hard failure. `output_shape_driver.c` is built by its own suite
  (`abi_failure.rs`)
- `support/mod.rs`, `cdriver/mod.rs` → the shared toolchain + fixture helpers
- `fixtures/` → the C driver objects (see above)
