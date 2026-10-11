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

#![doc = "Layout-frozen NIF (`enif_*`) and driver (`driver_*`/`erl_drv_*`) C ABI declarations."]
//!
//! `beam_abi` owns the **frozen foreign-function C ABIs** the Rust ERTS exposes to native code:
//! the NIF C ABI (workspace compatibility contract **A.3**) and the driver C ABI (**A.4**). It
//! reproduces the exact struct layouts, enum widths, function-pointer signatures and constants of
//! the legacy headers (`erts/emulator/beam/erl_nif.h`, `erl_nif_api_funcs.h`, `erl_drv_nif.h`,
//! `erl_driver.h`) so that a NIF/driver compiled against the *unmodified* OTP headers links against
//! Rust ERTS and behaves identically. This is the **link-time frozen-header seam** of Part II §C.
//!
//! The crate freezes the *shape* of the boundary (the types, the layouts, the term transmute, the
//! `ErlNifApi`/`ErlDrvApi` function tables) **and implements both host sides**: the `enif_*` bodies
//! live in [`nif::enif`] (one file per family — all `176` LP64 slots populated, the seven
//! C-variadic ones through the `csrc/nif_variadic.c` shim that `build.rs` compiles when a C
//! compiler is available) and the `driver_*`/`erl_drv_*` bodies in [`driver::drv`] (`108` of `111`
//! slots). Each side also has a `dlopen`-based loader ([`nif::loader`], [`driver::loader`]).
//!
//! Layouts are pinned by compile-time assertions in [`layout`]; a mismatch is a compile error.
//!
//! See `CONTEXT.md` for the ABI version targeted (`ERL_NIF_MAJOR_VERSION` 2 / minor 18,
//! `ERL_DRV_EXTENDED_MAJOR_VERSION` 3 / minor 3), the full frozen layout table, and the design
//! decisions (flag newtypes, opaque handles, the term transmute seam).
//!
//! The ABI version constants are declared **once, here** (the authoritative location per the crate
//! `CONTEXT.md` routing table) and re-exported from [`nif::types`] / [`driver::types`] so both the
//! root and the module paths resolve.

// The whole crate mirrors C identifiers (e.g. `ERL_NIF_TERM`, `ErlDrvSysInfo`).
#![allow(non_camel_case_types)]

use libc::c_int;

pub mod common;
pub mod driver;
pub mod layout;
pub mod nif;
pub mod sync;

// ---------------------------------------------------------------------------------------------
// ABI version constants (`erl_nif.h`, `erl_driver.h`)
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_MAJOR_VERSION` — NIF interface major version (`erl_nif.h`).
pub const ERL_NIF_MAJOR_VERSION: c_int = 2;

/// `ERL_NIF_MINOR_VERSION` — NIF interface minor version (`erl_nif.h`).
pub const ERL_NIF_MINOR_VERSION: c_int = 18;

/// `ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD` — the emulator refuses to load a NIF library with
/// a lower major version.
pub const ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD: c_int = 2;

/// `ERL_NIF_VM_VARIANT` — the VM variant a NIF library must match to be loaded.
///
/// The C macro is a string literal placed into `ErlNifEntry::vm_variant` (`const char *`); when a
/// NUL-terminated C string is required, append `\0` (`b"beam.vanilla\0"`).
pub const ERL_NIF_VM_VARIANT: &str = "beam.vanilla";

/// `ERL_NIF_MIN_ERTS_VERSION` — the minimum ERTS version a library built against this interface
/// requires.
pub const ERL_NIF_MIN_ERTS_VERSION: &str = "erts-14.0";

/// `ERL_DRV_EXTENDED_MARKER` — marks an `ErlDrvEntry` as having the extended fields.
///
/// C defines the literal `(0xfeeeeeed)` (an `unsigned int` because it exceeds `INT_MAX`); the
/// `ErlDrvEntry::extended_marker` field is an `int`, and the marker is only ever compared for
/// bit-pattern equality, so the constant is declared with the field's type (`c_int`).
pub const ERL_DRV_EXTENDED_MARKER: c_int = 0xfeee_eeed_u32 as c_int;

/// `ERL_DRV_EXTENDED_MAJOR_VERSION` — driver interface major version.
pub const ERL_DRV_EXTENDED_MAJOR_VERSION: c_int = 3;

/// `ERL_DRV_EXTENDED_MINOR_VERSION` — driver interface minor version.
pub const ERL_DRV_EXTENDED_MINOR_VERSION: c_int = 3;

/// `ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD` — the emulator refuses to load a driver with a
/// lower major version.
pub const ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD: c_int = 2;

// ---------------------------------------------------------------------------------------------
// Root re-exports — the term seam, the internal term, and the shared (`erl_drv_nif.h`) types
// ---------------------------------------------------------------------------------------------

pub use crate::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM, ERL_NIF_UINT};
pub use crate::nif::host::{NifEnv, NifHost};
pub use crate::driver::async_job::{AsyncCallback, AsyncJob};
pub use crate::driver::current_host::with_current_host;
pub use crate::driver::host::{DriverEnv, DriverHost, MonitorResult};
pub use crate::driver::drv::{monitor_from_identity, monitor_identity};
pub use beam_term::Eterm;

pub use crate::common::{
    ErlDirtyJobFlags, ErlDrvMonitor, ErlDrvSysInfo, ErlDrvThreadOpts, ErlNifSelectFlags, SysIOVec,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nif_abi_version_is_frozen() {
        assert_eq!(ERL_NIF_MAJOR_VERSION, 2);
        assert_eq!(ERL_NIF_MINOR_VERSION, 18);
        assert_eq!(ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD, 2);
        assert_eq!(ERL_NIF_VM_VARIANT, "beam.vanilla");
        assert_eq!(ERL_NIF_MIN_ERTS_VERSION, "erts-14.0");
    }

    #[test]
    fn driver_abi_version_is_frozen() {
        assert_eq!(ERL_DRV_EXTENDED_MARKER, 0xfeeeeeedu32 as c_int);
        assert_eq!(ERL_DRV_EXTENDED_MAJOR_VERSION, 3);
        assert_eq!(ERL_DRV_EXTENDED_MINOR_VERSION, 3);
        assert_eq!(ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD, 2);
    }

    #[test]
    fn root_re_exports_the_term_seam() {
        // Both the root path and the `nif::term` path must resolve to the same items.
        assert_eq!(
            crate::ERL_NIF_TERM::default(),
            crate::nif::term::ERL_NIF_TERM::default()
        );
        assert_eq!(nif_from_eterm(Eterm::nil()), ERL_NIF_TERM::default());
        assert_eq!(eterm_from_nif(ERL_NIF_TERM::default()), Eterm::nil());
    }
}
