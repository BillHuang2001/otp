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
  `cc_available()`, `skip()`, `probe_driver_name()` (a `libc`-only `dlopen` + `driver_init` read,
  used where a `beam_abi` dependency is not available to the test).
- `cdriver/mod.rs` — the shared C-driver helpers: `compile_fixture`/`fixture_so`/`load_driver`/
  `open_port`/`round_data`/`await_log_line`/`logged_port_command`/`LoggedPort` and the fixture
  builders (`variant_fixture_*`, `no_process_exit_variant_fixture_*`, `vector_fixture_so`,
  `both_fixture_so`).
- `fixtures/*.c` — the C driver objects. `host_driver.c` is the base; the others `#include` it
  after defining a `HOST_DRIVER_*_SLOT` override macro, so one file carries the whole vocabulary.

## The C fixtures (`fixtures/`)
- **`host_driver.c`** (`driver_name = "host_driver"`) — the base fixture. `start` parses
  `fd=<n>`, `log=<path>`, `upper` and `hdl`. Its integer `control` verbs (documented at the top of
  the file, the authoritative list) cover `driver_output`, timers, `driver_select` +
  `ERL_DRV_USE` release, `erl_drv_output_term`, `driver_async`, the driver-queue family and a
  `ERL_DRV_WRITE` arm:
  `19` `driver_enq`, `20` `driver_sizeq`, `21` `driver_peekq`, `22` `driver_deq`,
  `23` `driver_peekqv`, `24` `driver_select(WRITE)`. Verbs 19–23 answer `key=value` text with a
  trailing raw `bytes=` payload (the queue family's observation channel).
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

## Constraints
- **Skip loudly, never fail on a missing toolchain.** A suite needing `cc`/`erl_driver.h`/`erl`
  prints a `SKIP:` banner and returns; a *load* failure is never a skip (it means the `driver_*`
  symbols are missing from the test binary's dynamic symbol table).
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
bare `cargo`, never `cd` — use the wrapper, the sandbox `$HOME` is read-only). 18 suites, 173
passing; the full crate total is in the parent node's *Test Strategy*.

## Known Issues / Notes for Agents
- `driver_peekq`/`driver_peekqv` report `vlen == 1` on the Rust host (`AbiHost` publishes the whole
  contiguous queue as one iovec entry) where the oracle counts one entry per `driver_enq`.
- `AbiHost::pushq` flattens the queue (`front()` + clear + push), so byte order after a
  `driver_pushq` is not the C's; no fixture verb drives `driver_pushq` and no test asserts its
  order.
- The fixture's per-callback `log=<path>` is append-mode and per-run scratch directories keep runs
  isolated; `stop_select` cannot see `ErlDrvData`, so it uses a file-scope log path — one port per
  test.

## Routing Table
- `abi_driver.rs` → the end-to-end C-driver path (incl. the driver-queue family round-trip, the
  `outputv`-over-`output` preference and the empty-queue write-readiness case)
- `abi_async.rs`, `abi_vector.rs`, `abi_lifecycle.rs`, `abi_start_failure.rs` → the `driver_async`
  completion arms, the `outputv`-only entry, the port-exit adapter, a failing `start`
- `driver_table.rs`, `driver_registry_slots.rs` → the process-global driver registry
- `poller.rs`, `port.rs`, `port_task.rs`, `driver_seams.rs` → the reactor, the port, port-task
  scheduling, the driver-declared timer/port-exit seams
- `efile.rs`, `spawn.rs`, `inet.rs` → the built-in drivers on real files, real children, loopback TCP
- `signal.rs`, `cpu_time.rs`, `ddll.rs` → the OS-signal facility, the CPU-time facility, `dlopen`
- `fixture_builds.rs` → builds + loads every `fixtures/*.c` (a broken fixture is a hard failure)
- `support/mod.rs`, `cdriver/mod.rs` → the shared toolchain + fixture helpers
- `fixtures/` → the C driver objects (see above)
