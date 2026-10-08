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

//! Driver C ABI — the `ErlDrv*` types and the `driver_*`/`erl_drv_*` function table.
//!
//! Reproduces the layout of `erts/emulator/beam/erl_driver.h`. See the crate `CONTEXT.md` for the
//! frozen layout table.
//!
//! - [`types`] — the `#[repr(C)]` `ErlDrv*` structs/enums/flags, `ErlDrvEntry`, and constants.
//! - [`api`] — `ErlDrvApi`, the frozen `driver_*`/`erl_drv_*` function-pointer table.
//! - [`host`] — the driver host seam: `DriverHost` (the runtime services behind the `erl_drv_*`
//!   implementations) and `DriverEnv` (the concrete env behind the opaque `ErlDrvPort` a runtime
//!   passes to driver callbacks), plus the term-data-array decoder.
//! - [`current_host`] — the thread-local publication (`with_current_host`) that lets the frozen slots
//!   with **no** port operand (`add_driver_entry`/`remove_driver_entry`) reach a `DriverHost`.
//! - [`drv`] — the implemented `driver_*`/`erl_drv_*` host functions and the populated
//!   `driver_api_table()` they are published through.
//! - [`loader`] — the dynamic driver loader: `dlopen`/`dlsym`, the `ErlDrvEntry` load contract,
//!   and `DriverLibrary`.

pub mod api;
pub mod current_host;
pub mod drv;
pub mod host;
pub mod loader;
pub mod types;

pub use current_host::with_current_host;
