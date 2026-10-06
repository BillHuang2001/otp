//! Build script for `beam_abi` — export the dynamic symbol table of the test/binary link.
//!
//! The NIF C ABI (contract A.3) is a **link-time** seam: the runtime exports the `enif_*`
//! functions (and the `enif_*` globals a NIF library resolves against) as *dynamic* symbols, and a
//! NIF `.so` loaded at run time resolves them out of the loading process. By default Rust links its
//! test/binaries with `-Wl,--no-export-dynamic`, so those symbols would be absent from the dynamic
//! symbol table and a loaded `.so` could not bind to them.
//!
//! `-Wl,--export-dynamic` (the linker spelling of the `-rdynamic` codegen flag) adds every global
//! symbol to `.dynsym`. Applying it to the **test** targets (not to `libbeam_abi`, which is not
//! where the `enif_*` implementations live) is exactly what a later step's "load a real NIF `.so`
//! and call it" test needs.
//!
//! Note: the `cargo::rustc-link-arg-bins` counterpart is deliberately *not* emitted — this package
//! has no `[[bin]]` target, and Cargo hard-errors on a `*-bins` instruction when there is none
//! ("the package ... does not have a bin target"). The test target is the only link this crate
//! needs the flag for.
//!
//! Nothing else is configured here — in particular no `rerun-if-changed`, since this script has no
//! input files.

fn main() {
    println!("cargo::rustc-link-arg-tests=-Wl,--export-dynamic");
}
