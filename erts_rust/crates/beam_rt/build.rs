//! Build script for `beam_rt` — export the dynamic symbol table of the binary (and test) link.
//!
//! The NIF C ABI (contract A.3) is a **link-time** seam: the runtime exports the `enif_*`
//! functions (and the `enif_*` globals a NIF library resolves against) as *dynamic* symbols, and a
//! NIF `.so` loaded at run time resolves them out of the loading process. By default Rust links its
//! binaries with `-Wl,--no-export-dynamic`, so those symbols would be absent from `.dynsym` and a
//! loaded `.so` could not bind to them (`dlopen` would fail with an undefined symbol).
//!
//! `-Wl,--export-dynamic` (the linker spelling of the `-rdynamic` codegen flag) adds every global
//! symbol to `.dynsym`. Both target kinds need it here:
//!
//! * `-bins` — the `beam_rt` executable is what actually loads a NIF via `erlang:load_nif/2`
//!   (`tests/nif.rs` drives that binary end to end). This package **has** a `[[bin]]` target, so
//!   the instruction is legal (unlike in `beam_abi`, which has none and would hard-error).
//! * `-tests` — the crate's integration tests link their own harness executable and would
//!   otherwise not export the symbols.
//!
//! Exporting the symbols is only half of it: the linker also needs a reason to *keep* the object
//! files that define them. `beam_abi` holds the `enif_*` implementations behind a `#[used]` static
//! API table; `NifRegistry::new` (in `src/nif.rs`) takes a `std::hint::black_box` reference to
//! `beam_abi::nif::enif::enif_api_table`, which anchors that table's codegen unit in the link.
//! `tests/nif.rs` asserts the outcome with `nm -D`.
//!
//! Nothing else is configured here — in particular no `rerun-if-changed`, since this script has no
//! input files.
fn main() {
    println!("cargo::rustc-link-arg-bins=-Wl,--export-dynamic");
    println!("cargo::rustc-link-arg-tests=-Wl,--export-dynamic");
}
