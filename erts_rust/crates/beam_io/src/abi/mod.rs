//! The driver **C-ABI host layer**: a loaded `ErlDrvEntry` presented as a
//! [`Driver`](crate::driver::Driver).
//!
//! Crate `beam_abi` freezes the *shape* of the driver C ABI (contract **A.4**): the `#[repr(C)]`
//! `ErlDrv*` types ([`beam_abi::driver::types`]), the frozen [`ErlDrvApi`] function table
//! ([`beam_abi::driver::api`]), the implemented `driver_*`/`erl_drv_*` host functions
//! ([`beam_abi::driver::drv`]), the runtime seam they call through
//! ([`beam_abi::driver::host::DriverHost`] + [`beam_abi::driver::host::DriverEnv`]) and the
//! dynamic loader ([`beam_abi::driver::loader`]). What `beam_abi` deliberately does **not** have is
//! a *port*: registration with a poller, the input/output byte queues, timers and the port-task
//! round are this crate's ([`beam_io`](crate)). This module is the bridge.
//!
//! It is small on purpose and has exactly four moving parts:
//!
//! * [`host`] — [`PortHost`], the one runtime service this crate cannot supply (atom interning, so
//!   that a term built by a driver lands in the *runtime's* atom table), and [`AbiHost`], the
//!   [`DriverHost`](beam_abi::driver::host::DriverHost) implementation that owns the port's
//!   persistent C-ABI state (the `driver_select` registration map, the `driver_set_timer` deadline
//!   and the `driver_async` job set) and reaches the live port through the
//!   [`DriverContext`](crate::driver::DriverContext) installed for the duration of one callback.
//! * [`job`] — [`AsyncJobs`], the `driver_async` bookkeeping (one worker thread per job, a queue
//!   position, and the completion channel [`DriverPort::poll_async`] drains to run `ready_async`).
//! * [`port`] — [`DriverPort`], the adapter that implements [`Driver`](crate::driver::Driver) over a
//!   loaded [`DriverLibrary`](beam_abi::driver::loader::DriverLibrary): it maps `start`/`output`/
//!   `ready_input`/`ready_output`/`timeout`/`control`/`stop` onto the driver's callbacks, installs
//!   the port context around each call, and forwards the driver's own `driver_*` calls back into
//!   [`AbiHost`].
//! * [`table`] — [`DriverTable`], the **process-global** driver registry (the C's `driver_list`)
//!   plus its port-creation service: the `beam_io` half of `add_driver_entry` /
//!   `remove_driver_entry` / `driver_lock_driver` / `driver_create_port`. Those four `beam_abi`
//!   `ErlDrvApi` slots are `None` today; wiring them needs four new *defaulted*
//!   [`DriverHost`](beam_abi::driver::host::DriverHost) methods forwarding to this table (a
//!   per-port host reaches the process-global table, exactly as a driver callback reaches the
//!   global list in the C):
//!
//!   ```text
//!   fn add_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int;            // -> table.add_entry(de)
//!   fn remove_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int;         // -> table.remove_entry(de)
//!   fn lock_driver(&mut self) -> c_int;                                       // -> table.lock(<this port's driver name>)
//!   fn create_port(&mut self, creator: ErlDrvPort, connected: ErlDrvTermData,
//!                  name: *mut c_char, drv_data: ErlDrvData) -> ErlDrvPort;   // -> table.create_port(...)
//!   ```
//!
//!   `table` (a `&DriverTable`, defaulting to [`DriverTable::global`]) would become a **fifth**
//!   optional hook alongside [`PortHost`]: `driver_create_port` additionally needs the runtime's
//!   [`PortRegistry`](crate::PortRegistry)/[`Poller`](crate::Poller) and a `Box<dyn PortHost>` for
//!   the new port, which is why the hook belongs on [`PortHost`] (the runtime-services seam) rather
//!   than on the per-port `DriverHost`. `AbiHost` knows neither its own driver's name nor the
//!   registry, so a `driver_lock_driver`/`driver_create_port` forwarding also needs the port's
//!   registered driver name handed to [`AbiHost`] at construction.
//!
//! # The `ErlDrvPort` a driver sees
//!
//! `beam_abi` defines the port handle a driver callback receives as the pointer
//! [`DriverEnv::as_port`](beam_abi::driver::host::DriverEnv::as_port) returns, recovered by
//! [`DriverEnv::from_port`](beam_abi::driver::host::DriverEnv::from_port). [`DriverPort`] therefore
//! keeps its [`DriverEnv`](beam_abi::driver::host::DriverEnv) **boxed** and pointing at its boxed
//! [`AbiHost`], so the handle is the **same pointer for the port's whole life** — which is what the
//! C guarantees (`&prt->drv_port` never moves) and what lets a driver stash it.
//!
//! # Built-in drivers and loaded `.so`s are the same thing here
//!
//! A built-in driver ([`crate::drivers::FdDriver`] &c.) implements
//! [`Driver`](crate::driver::Driver) directly; a loaded C driver reaches the same trait through
//! [`DriverPort`]. Both present as a `Box<dyn Driver>` and open through the same
//! [`PortRegistry::open`](crate::PortRegistry::open)/[`open_named`](crate::PortRegistry::open_named)
//! path — see [`make_loaded`] for the `.so` side.
//!
//! # `unsafe`
//!
//! Every module in this directory is an explicitly-marked `unsafe` module (workspace
//! `../CONTEXT.md` §3): it holds raw `ErlDrv*` pointers, calls foreign code and re-enters the
//! runtime through the opaque `ErlDrvPort`. Every `unsafe` block carries a `// SAFETY:` rationale.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_driver.h` (the ABI),
//! `erts/emulator/beam/io.c` (`call_driver_control`, `driver_output`, `driver_read_timer`,
//! `async_ready`, `init_driver`), `erts/emulator/sys/unix/erl_check_io.c` (`driver_select`) and
//! `erts/emulator/beam/erl_async.c` (the async worker).

pub mod host;
pub mod job;
pub mod port;
pub mod table;

pub use host::{AbiHost, DriverState, LocalAtomPortHost, PortHost};
pub use job::AsyncJobs;
pub use port::{make_loaded, DriverPort};
pub use table::{DriverTable, RegisteredDriver};
