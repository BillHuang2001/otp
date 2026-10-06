//! The dynamic NIF library loader — `dlopen`/`dlsym` + the `ErlNifEntry` load-time contract.
//!
//! Placeholder: this module will hold the `erlang:load_nif/2` load path — resolving `nif_init`,
//! validating the [`ErlNifEntry`](super::types::ErlNifEntry) (version, variant, module-name and
//! function checks) and installing the function table. Added in a later step — it declares no items
//! yet.
