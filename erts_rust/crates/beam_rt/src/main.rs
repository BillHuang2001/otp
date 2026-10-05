//! The `beam_rt` executable: a thin shim over [`beam_rt::cli::run`].
//!
//! Every piece of logic lives in the library so it is unit- and integration-testable;
//! this file only forwards the command line (minus `argv[0]`) and turns the returned
//! exit code into the process status. See `beam_rt::cli` for the supported switch
//! subset (contract A.6) and `../CONTEXT.md` for the frozen surface.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(beam_rt::cli::run(args));
}
