#![doc = "Layout-frozen NIF (enif_*) and driver (driver_*/erl_drv_*) C ABI declarations."]

//! The foreign-function C ABIs exposed by the runtime.
//!
//! Implements compatibility contracts Part I §A.3 (NIF C ABI) and §A.4 (driver C
//! ABI) exactly as frozen by `../erts/emulator/beam/erl_nif.h` and `erl_driver.h`.
//! Part II §C describes this as the link-time frozen-header seam: struct layouts
//! must not change, so every type here is `#[repr(C)]` with static assertions.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_abi() -> &'static str {
    "beam_abi"
}
