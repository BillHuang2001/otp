//! The `bench` binary: a thin wrapper around the library.
//!
//! Parse the args, dispatch the command, and map the outcome onto the process exit
//! code (`0` success; `2` usage/env/IO/schema error). All logic lives in the library so
//! it is testable.

#![forbid(unsafe_code)]

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match bench::cli::parse(&args) {
        Ok(command) => match bench::run::dispatch(&command) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("bench: {err}");
                err.exit_code()
            }
        },
        Err(err) => {
            eprintln!("bench: {err}");
            eprintln!();
            eprintln!("{}", bench::cli::usage());
            err.exit_code()
        }
    };
    exit(code);
}
