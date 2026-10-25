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
  `open_named_port`/`round_data`/`data_of`/`reads_of`/`closed_of`/`await_data`/`drain_data`/
  `await_trailing_data`/`ASYNC_JOB_FRAMES`/`is_async_job_frame`/`drive_until`/
  `await_log_line`/`logged_port_command`/`LoggedPort` and the fixture builders
  (`fixture_so`, `variant_fixture_*`, `no_process_exit_variant_fixture_*`, `vector_fixture_so`,
  `both_fixture_so`, `shape_fixture_*`).
  `round_data` asserts a round reported **exactly one** `Data` (the no-coalescing rule for a
  call site that makes one `driver_output*` call); `drain_data` returns every frame the rounds
  report until a round reports none, and `await_trailing_data` collects frames until `expected`
  is the **newest** one — the shape a `driver_async` site needs (see *Known Issues*). The
  `soft_busy_fixture_source`/`soft_busy_fixture_so` builders are the `ERL_DRV_FLAG_SOFT_BUSY` twin
  `abi_busy.rs`'s `[force]` rows need.
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
- **`host_driver_soft_busy.c`** — `HOST_DRIVER_DRIVER_FLAGS_SLOT = ERL_DRV_FLAG_SOFT_BUSY`, the
  flagged twin of the base entry (zero flags): with the flag a `[force]` command bypasses the port's
  busy state and is written, without it the same command is `error:notsup`.
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
bare `cargo`, never `cd` — use the wrapper, the sandbox `$HOME` is read-only). 26 suites, **251
passing / 0 failing**; the whole `-p beam_io` run is **1328 passed / 0 failed / 2 ignored** over **28
test binaries** (1 lib + 26 integration + 1 doctest binary), i.e. **1074** lib tests + 251
integration tests + 5 doctests (3 passing, 2 `#[ignore]`d). Per file:

| Suite | Tests | Suite | Tests |
|---|---|---|---|
| `abi_driver.rs` | 23 | `spawn.rs` | 30 |
| `abi_async.rs` | 8 | `signal.rs` | 21 |
| `abi_vector.rs` | 7 | `efile.rs` | 16 |
| `driver_output2.rs` | 6 | `ram_file.rs` | 12 |
| `driver_table.rs` | 5 | `ddll.rs` | 11 |
| `driver_seams.rs` | 7 | `poller.rs` | 11 |
| `abi_lifecycle.rs` | 3 | `cpu_time.rs` | 8 |
| `driver_registry_slots.rs` | 3 | `fixture_builds.rs` | 9 |
| `abi_busy.rs` | 8 | `inet.rs` | 10 |
| `abi_start_failure.rs` | 2 | `inet_async.rs` | 8 |
| `abi_failure.rs` | 11 | `active_mode.rs` | 10 |
| `inet_accept.rs` | 7 | | |
| `port_task.rs` | 5 | `udp_inet.rs` | 6 |
| | | `port.rs` | 4 |

`active_mode.rs` (10) is the **`{active, …}` input channel** through a real `PortRegistry` port and a
real `Poller` round, with the traffic supplied by an **external** `std::net::UdpSocket`/
`std::net::TcpStream` (never a second VM): ports open through `PortRegistry::open_named`, verbs go
through the numeric `prim_inet` opcodes (`Op + 0x03f1a300`) — plus the named `accept` verb for the
stream half — and every round is `poll` → `handle_events` → `execute_port_tasks` →
`take_active_inputs`/`take_completions`. Rows: `{active,true}` UDP with the sender's seven-byte
packed address as the read header and the payload as the body; `{active,false}` reporting nothing
while `PACKET_REQ_RECV`(60) still completes with the same datagram (the passive-path regression
pin, incl. its `[0]einval` operand guard); `{active,once}` delivering one datagram, silently, with
no `Passive`; `{active,2}` delivering two datagrams then exactly one `Passive` and nothing more;
passive→`{active,true}` delivering the already-buffered datagram (the re-arm's synthesized edge);
`{active,true}` TCP delivering the peer's bytes with an **empty** header; `{packet,2}` TCP
delivering the **stripped** body; an `ActiveInputEvent::Error` (a peer reset under
`{show_econnreset,true}`) drained **before** the close report of the same round; two armed datagram
ports reported in **port-number order** by one destructive drain; and the channel staying empty for a
passive port and after a close. Waits are
conditions (one bounded poll slice per round under a 10 s hang guard), never wall-clock budgets, and
assertions are on content/sets, never on round counts.
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
  `a_command_answers_its_caller_tag_verbatim` (the `prim_inet` caller tag reaching the completion
  byte-for-byte),
  `an_accept_completion_hands_over_a_descriptor_the_runtime_can_adopt` (the descriptor hand-off;
  `INET_REQ_FDOPEN` adoption is a crate-local route to the same state the C reaches internally) and
  `port_notifications_stay_exactly_two_variants` (the compile-time two-arm match).
- **`udp_inet.rs` (6)** — `the_code_60_recv_is_an_async_request_completed_by_a_later_round` is the
  datagram half, byte-compared against a real raw `udp_inet` port on every run-specific-independent
  row: the `[1,0,0]` id reply with nothing staged, `[0]ealready` for a second concurrent receive
  (which still consumes an id, so the next request is `[1,0,2]`), `[0]exbadport` socket-less,
  `[0]einval` for a `len != 8` operand, one `InetAsyncResult::Data` completion at the
  `execute_port_tasks` drive point with `read.flattened()` = `[1,PortHi,PortLo,A,B,C,D,…data]` (the
  address prefix is shape-compared, each side checking its own sender's port), and `{error, closed}`
  exactly once for a receive pending at close. The file's other oracle case
  (`the_covered_control_replies_agree_with_the_erl_oracle`) is the open/bind/setopts/getopts/close
  gate. `an_explicit_bind_port_whose_low_byte_is_the_old_delimiter_agrees_byte_exactly` is the
  **dump-framing** row: both sides bind the **same** explicit port, low byte `0x7C` (`|`), so the
  `bind` reply `[1, hi, 0x7C]` is compared byte-exactly; the candidate-port loop retries
  allocation only.
  **Framing rule for every oracle dump in `tests/`:** a raw-byte dump is **length-framed**
  (`<<(byte_size(X)):32/big, X/binary>>` + `parse_reply_dump`, as here and in
  `src/drivers/inet/oracle_dump.rs`) or its replies are re-encoded as ASCII decimal/hex text first —
  never raw bytes split on a delimiter (any byte, `0x7C` included, is legitimate payload).

**`abi_failure.rs` (11)** is the one suite that pins the **`driver_output*` shapes and the
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
- **A `driver_async` job's frames are an asynchronous delivery, so their *count* at a call site is
  not fixed.** The fixture's worker thread `driver_output`s `"A"` and its `ready_async`
  `driver_output`s `"H"`; a worker-thread delivery only lands when it coincides with a driver
  callback's installed port context, so how many of the two are owed after a drive point varies run
  to run (both is legitimate — the C delivers from any thread, `erts/emulator/beam/io.c:6387-6446`).
  Assert the **sequence** (`drain_data`/`await_trailing_data` + `is_async_job_frame`), never the
  count; `round_data`'s "exactly one" assert belongs to call sites that make one `driver_output*`
  call themselves.
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
- **The read path's ordering rule — a complete buffered unit wins over a pending peer close — is
  pinned at the `src/drivers/inet/write_then_close_tests.rs` level, not here**; these suites see only
  its consequence (the `{ok, Data}` a queued receive answers ahead of the `{error, closed}` the
  *following* operations are owed), so a change to that ordering belongs in that unit suite.
## Routing Table
- `abi_driver.rs` → the end-to-end C-driver path (incl. the driver-queue family round-trip, the
  `outputv`-over-`output` preference, the empty-queue write-readiness case, the `NULL`-`*rbuf`
  `control` reply and the silent-driver output pin)
- `driver_output2.rs` → the `driver_output2`/`driver_output_binary` **header/body split** through a
  real `PortRegistry`: one driver call = one `ReadMessage { header, body }` notification, the empty
  header vs `body: None` distinction, two calls in one callback = two messages in arrival order,
  `input_len` counting the header, the `{line, N}` frame header stamp, and the flat
  `DriverContext::input()` view
- `abi_busy.rs` → the `set_busy_port` state half (the raw API-table hook + the fixture's `busy`
  token and verbs 25/26) **and** the port layer's busy-port decision table over that real loaded
  driver: an idle port really writing, the `busy` token making the first command `Busy` with nothing
  queued, `[nosuspend]` on it `Busy` too, verb 26 letting the command through, `[force]` raising
  `notsup` without `ERL_DRV_FLAG_SOFT_BUSY` and reaching the driver with it
  (`fixtures/host_driver_soft_busy.c`)
- `abi_async.rs`, `abi_vector.rs`, `abi_lifecycle.rs`, `abi_start_failure.rs` → the `driver_async`
  completion arms, the `outputv`-only entry, the port-exit adapter, a failing `start`
- `abi_failure.rs` → the `driver_output*` shapes + the driver-failure channel over
  `fixtures/output_shape_driver.c` (one `ReadMessage` per call, the `body: None`/`Some([])`
  distinction, every `FailureReason`, `driver_exit`'s no-signal shape, the
  data-before-close ordering, the post-failure refusal, the id release, and the live 16-row
  oracle table)
- `driver_table.rs`, `driver_registry_slots.rs` → the process-global driver registry
- `poller.rs`, `port.rs`, `port_task.rs`, `driver_seams.rs` → the reactor, the port, port-task
  scheduling, the driver-declared timer/port-exit seams. A driver-seam suite's port may be **parked
  rather than finished** after `close` — the lying-write row closes a port whose queue still holds the
  bytes the driver falsely counted as written — and `PortRegistry::is_closing(id)` is how a suite
  observes that (a parked port is still a live port with its number allocated, not a leak).
- `inet_async.rs` → the `tcp_inet` **asynchronous-op queue + completion channel** through
  `PortRegistry` (numeric opcodes, the single receive slot, the caller tag, the accept hand-off and
  its `INET_REQ_FDOPEN` adoption, the `{error, closed}` teardown); three `erl`-oracle cases
- `efile.rs`, `spawn.rs`, `inet.rs` → the built-in drivers on real files, real children, loopback TCP
  (`spawn.rs` also cross-checks the port-level `[line]`/`[binary,{line,N}]` framing against the oracle).
  `inet.rs`'s `a_reactor_observed_peer_close_quiesces_and_owes_the_next_recv_its_closed` is the
  reactor-level pin of the **read-selection** rule (`src/drivers/inet/CONTEXT.md` › *The
  read-selection invariant*): the edge-triggered poller does read a passive socket's end-of-file, but
  that read is `ReadReport::Unselected`, so it owes the owner **no** notification, leaves
  `port_closed` `false` and `INET_REQ_GETSTATUS`(16) reporting `INET_STATE_CONNECTED` (`5`), and the
  reactor settles (five consecutive empty bounded polls — a dead descriptor would report every poll).
  The next `recv` is the read the C would have made: it answers `closed` on its first call then
  `enotconn`, and only there does the registration go (`poller.is_empty()`, `INET_STATE_CLOSED` = `0`,
  the default `{exit_on_close,true}`). The opcode is sent as `16 + CONTROL_MAGIC`; a bare `16` wraps
  unsigned and is the oracle's `[0]exbadport`.
- `spawn.rs`'s `a_stalled_write_stays_queued_and_arms_write_readiness` is the integration-level pin of
  the C's **deferred port close** (`io.c:3946-3956` + `flush_port` `io.c:3659-3700`): a `sleep` child
  never reads its stdin, so the write stalls with the unwritable remainder still queued and write
  readiness armed, and `PortRegistry::close` **parks** the port in `PortState::Closing` with that
  remainder retained (`output_len() > 0`), an `OUTPUT` task pending, both poller registrations still
  armed, its number still allocated, `stop` not run and no `Closed` reported; a command on it is
  `badarg` and queues nothing, a second `close` is idempotent (nothing written, nothing torn down),
  and a following round that makes no progress leaves it parked and owes the owner no `Closed`. The
  fully-drained closes in `port.rs`/`port_task.rs`/`efile.rs` still deregister inside the call — that
  is the same contract's synchronous arm, not a discarded queue.
- `udp_inet.rs`, `ram_file.rs` → the other two built-ins through the real port layer
  (`DriverRegistry::with_builtins` + `PortRegistry` + `Poller`): a loopback UDP datagram round trip
  (`udp_inet`) and the packed opcode/`control` command grammar (`ram_file_drv`), each with an
  `erl`-oracle cross-check over the reply bytes that skips loudly without `erl`. `udp_inet` also
  pins the **`PACKET_REQ_RECV`(60) asynchronous** receive (id reply, `ealready`/`exbadport`/`einval`
  ladder, the `Data` completion with the packed source address as the read's header, the
  `{error, closed}` teardown) against the real datagram driver, and the **length-framed dump** with
  an explicit low-byte-`0x7C` bind compared byte-exactly
- `active_mode.rs` → the **`{active, …}` input channel** (`ActiveInput`/`ActiveInputEvent`) at the
  reactor + registry level, armed through `setopts` and fed by an external `std::net` peer:
  `{active,true}`/`{active,once}`/`{active,2}` on `udp_inet` (packed source-address header, the one
  `Passive` of a spent counter, the silent `once` flip), the passive floor + its `PACKET_REQ_RECV`(60)
  regression pin, the passive→active re-arm, `tcp_inet` `{active,true}` and `{packet,2}` (empty header,
  stripped body), two-port port-number ordering with a destructive drain, and the empty channel for a
  passive port and after a close
- `signal.rs`, `cpu_time.rs`, `ddll.rs` → the OS-signal facility, the CPU-time facility, `dlopen`
- `fixture_builds.rs` → builds + loads the `host_driver`-family fixtures (the base, the two
  `ready_async`/`process_exit` variants, the `soft_busy` flag variant, the `vector` and `both`
  write-side shapes) plus the negative cases (a `cc` failure is reported, a missing object is an
  error, the OTP header is locatable); a broken fixture is a hard failure. `output_shape_driver.c` is
  built by its own suite (`abi_failure.rs`)
- `support/mod.rs`, `cdriver/mod.rs` → the shared toolchain + fixture helpers
- `fixtures/` → the C driver objects (see above)
- **`inet_accept.rs` (7)** — the **TCP accept chain** end to end over a foreign `std::net::TcpStream` peer:
  two successive loopback connections accepted through the multi-op list and adopted (`from_socket`), the
  completion arriving as `InetAsyncResult::Accepted { fd }`, the sticky `INET_F_MULTI_CLIENT` `GETSTATUS`
  bits, the `[0]einval` ladder, the per-port accept timer (`[500, 50]` reports id 0 then id 1), and the
  arm/disarm readiness cycle (`poll` + `handle_events` alone completes a connection that arrived while
  nothing was queued). Detail: `src/drivers/inet/accept_tests.rs` rustdoc; the runtime's obligations
  (second port, `{inet_async, Listener, Id, {ok, NewPort}}`, `tcp_inet_copy` inheritance) are the crate
  doc's *Handoffs* (0).
