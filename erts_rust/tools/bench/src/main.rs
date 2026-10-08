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
