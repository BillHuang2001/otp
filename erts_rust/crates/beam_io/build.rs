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

//! Build script for `beam_io` — export the dynamic symbol table of the test link.
//!
//! The driver C ABI (contract A.4) is a **link-time** seam, exactly like the NIF C ABI
//! (contract A.3): a driver `.so` loaded at run time leaves `driver_*`/`erl_drv_*` **undefined**
//! and resolves them out of the loading process. The `#[no_mangle]` implementations of those
//! functions live in `beam_abi::driver::drv` and are pulled into the runtime's link by
//! [`beam_abi::driver::drv::driver_api_table`](https://docs.rs/beam_abi) (`src/abi/port.rs`
//! references it, and `beam_io`'s test targets reference `DriverPort`), but by default Rust links
//! its test/binaries with `-Wl,--no-export-dynamic`, so they would be absent from `.dynsym` and a
//! loaded driver `.so` could not bind to them.
//!
//! `-Wl,--export-dynamic` (the linker spelling of `-rdynamic`) adds every global symbol to
//! `.dynsym`. Applying it to the **test** targets is what the end-to-end "load a real C driver
//! `.so` and call it" test needs (`tests/fixtures/host_driver.c` + `tests/support/mod.rs`, owned
//! by a sibling task).
//!
//! Note: the `cargo::rustc-link-arg-bins` counterpart is deliberately *not* emitted — this package
//! has no `[[bin]]` target, and Cargo hard-errors on a `*-bins` instruction when there is none.
//! Nothing else is configured here (in particular no `rerun-if-changed`: this script has no input
//! files).

fn main() {
    println!("cargo::rustc-link-arg-tests=-Wl,--export-dynamic");
}
