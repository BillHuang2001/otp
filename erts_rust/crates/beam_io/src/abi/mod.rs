// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

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
//!   persistent C-ABI state (the `driver_select` registration map — including the two
//!   `ERL_DRV_USE` release bits `driver_select`'s `mode` may carry, and the queue of
//!   `stop_select` callbacks a released selection still owes — the `driver_set_timer` deadline and
//!   the `driver_async` job set) and reaches the live port through the
//!   [`DriverContext`](crate::driver::DriverContext) installed for the duration of one callback.
//! * [`job`] — [`AsyncJobs`], the `driver_async` bookkeeping (a bounded, process-global worker
//!   pool shared by every port, a queue position per job, and the per-port completion channel
//!   [`DriverPort::poll_async`] drains to run `ready_async`). A port with outstanding jobs is driven
//!   from the port-task round (see [`crate::PortRegistry::execute_port_tasks`]), so a completion's
//!   `driver_output` surfaces in the same round.
//! * [`port`] — [`DriverPort`], the adapter that implements [`Driver`](crate::driver::Driver) over a
//!   loaded [`DriverLibrary`](beam_abi::driver::loader::DriverLibrary): it maps `start`/`output`/
//!   `ready_input`/`ready_output`/`timeout`/`control`/`stop` onto the driver's callbacks, installs
//!   the port context around each call, forwards the driver's own `driver_*` calls back into
//!   [`AbiHost`], and runs the `stop_select` release callback of an `ERL_DRV_USE` selection — on
//!   the de-selecting `driver_select`, at port teardown (`stop`) and on `Drop`.
//! * [`table`] — [`DriverTable`], the **process-global** driver registry (the C's `driver_list`)
//!   plus its port-creation service. It is where the [`AbiHost`] forwards the four `beam_abi`
//!   `ErlDrvApi` slots that act on structure outside one port — `add_driver_entry`,
//!   `remove_driver_entry`, `driver_lock_driver` and `driver_create_port`:
//!
//!   * `add_driver_entry`/`remove_driver_entry` delegate to [`DriverTable::global`]'s
//!     `add_entry`/`remove_entry`. Because those two slots carry **no** port operand, they reach
//!     their host through the thread-local publication `beam_abi::with_current_host`, which
//!     [`DriverPort`] installs around every callback it runs (see [`port`]'s module docs). An
//!     `add_driver_entry` whose `init` refuses the entry answers a positive non-zero code (the C's
//!     `void` wrapper discards it); `remove_driver_entry` forwards the table's own `1`/`0`/`-1`.
//!   * `driver_lock_driver` consults the **port's own** driver, not a name lookup (the C's
//!     `prt->drv_ptr->handle`). [`AbiHost`] therefore records the port's registered driver identity
//!     (`AbiHost::for_driver`/`with_driver`, set by [`DriverPort::from_library`]), so a dynamically
//!     loaded port answers `0` and a bare host answers the C's statically-linked `-1`.
//!   * `driver_create_port` answers the invalid-port sentinel
//!     ([`beam_abi::driver::types::ERTS_INVALID_ERL_DRV_PORT`]), the C's single failure answer: a
//!     driver callback here already holds a `&mut` into the port registry (re-entering
//!     [`PortRegistry`](crate::PortRegistry)'s `open` would alias it, and could even invalidate the
//!     live `&mut Port`), and the new port would additionally need a `Box<dyn PortHost>` only
//!     `beam_rt` can supply. See the method docs on [`AbiHost`] for the full rationale.
//!
//!   So the runtime owns the driver list and the port table (as the C's emulator does), [`AbiHost`]
//!   is the per-port bridge to them, and no slot fabricates a success it cannot honour.
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
